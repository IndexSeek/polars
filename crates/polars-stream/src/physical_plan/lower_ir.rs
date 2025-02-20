use std::sync::Arc;

use polars_core::frame::DataFrame;
use polars_core::prelude::{InitHashMaps, PlHashMap, PlIndexMap};
use polars_core::schema::Schema;
use polars_error::{polars_ensure, PolarsResult};
use polars_io::RowIndex;
use polars_plan::plans::expr_ir::{ExprIR, OutputName};
use polars_plan::plans::{AExpr, FileScan, FunctionIR, IRAggExpr, IR};
use polars_plan::prelude::{FileType, SinkType};
use polars_utils::arena::{Arena, Node};
use polars_utils::itertools::Itertools;
use slotmap::SlotMap;

use super::{PhysNode, PhysNodeKey, PhysNodeKind};
use crate::physical_plan::lower_expr::{
    build_select_node, is_elementwise_rec_cached, lower_exprs, ExprCache,
};

fn build_slice_node(
    input: PhysNodeKey,
    offset: i64,
    length: usize,
    phys_sm: &mut SlotMap<PhysNodeKey, PhysNode>,
) -> PhysNodeKey {
    if offset >= 0 {
        let offset = offset as usize;
        phys_sm.insert(PhysNode::new(
            phys_sm[input].output_schema.clone(),
            PhysNodeKind::StreamingSlice {
                input,
                offset,
                length,
            },
        ))
    } else {
        todo!()
    }
}

fn build_filter_node(
    input: PhysNodeKey,
    predicate: ExprIR,
    expr_arena: &mut Arena<AExpr>,
    phys_sm: &mut SlotMap<PhysNodeKey, PhysNode>,
    expr_cache: &mut ExprCache,
) -> PolarsResult<PhysNodeKey> {
    let predicate = predicate.clone();
    let cols_and_predicate = phys_sm[input]
        .output_schema
        .iter_names()
        .cloned()
        .map(|name| {
            ExprIR::new(
                expr_arena.add(AExpr::Column(name.clone())),
                OutputName::ColumnLhs(name),
            )
        })
        .chain([predicate])
        .collect_vec();
    let (trans_input, mut trans_cols_and_predicate) =
        lower_exprs(input, &cols_and_predicate, expr_arena, phys_sm, expr_cache)?;

    let filter_schema = phys_sm[trans_input].output_schema.clone();
    let filter = PhysNodeKind::Filter {
        input: trans_input,
        predicate: trans_cols_and_predicate.last().unwrap().clone(),
    };

    let post_filter = phys_sm.insert(PhysNode::new(filter_schema, filter));
    trans_cols_and_predicate.pop(); // Remove predicate.
    build_select_node(
        post_filter,
        &trans_cols_and_predicate,
        expr_arena,
        phys_sm,
        expr_cache,
    )
}

#[recursive::recursive]
pub fn lower_ir(
    node: Node,
    ir_arena: &mut Arena<IR>,
    expr_arena: &mut Arena<AExpr>,
    phys_sm: &mut SlotMap<PhysNodeKey, PhysNode>,
    schema_cache: &mut PlHashMap<Node, Arc<Schema>>,
    expr_cache: &mut ExprCache,
    cache_nodes: &mut PlHashMap<usize, PhysNodeKey>,
) -> PolarsResult<PhysNodeKey> {
    // Helper macro to simplify recursive calls.
    macro_rules! lower_ir {
        ($input:expr) => {
            lower_ir(
                $input,
                ir_arena,
                expr_arena,
                phys_sm,
                schema_cache,
                expr_cache,
                cache_nodes,
            )
        };
    }

    let ir_node = ir_arena.get(node);
    let output_schema = IR::schema_with_cache(node, ir_arena, schema_cache);
    let node_kind = match ir_node {
        IR::SimpleProjection { input, columns } => {
            let columns = columns.iter_names_cloned().collect::<Vec<_>>();
            let phys_input = lower_ir!(*input)?;
            PhysNodeKind::SimpleProjection {
                input: phys_input,
                columns,
            }
        },

        IR::Select { input, expr, .. } => {
            let selectors = expr.clone();
            let phys_input = lower_ir!(*input)?;
            return build_select_node(phys_input, &selectors, expr_arena, phys_sm, expr_cache);
        },

        IR::HStack { input, exprs, .. }
            if exprs
                .iter()
                .all(|e| is_elementwise_rec_cached(e.node(), expr_arena, expr_cache)) =>
        {
            // FIXME: constant literal columns should be broadcasted with hstack.
            let selectors = exprs.clone();
            let phys_input = lower_ir!(*input)?;
            PhysNodeKind::Select {
                input: phys_input,
                selectors,
                extend_original: true,
            }
        },

        IR::HStack { input, exprs, .. } => {
            // We already handled the all-streamable case above, so things get more complicated.
            // For simplicity we just do a normal select with all the original columns prepended.
            //
            // FIXME: constant literal columns should be broadcasted with hstack.
            let exprs = exprs.clone();
            let phys_input = lower_ir!(*input)?;
            let input_schema = &phys_sm[phys_input].output_schema;
            let mut selectors = PlIndexMap::with_capacity(input_schema.len() + exprs.len());
            for name in input_schema.iter_names() {
                let col_name = name.clone();
                let col_expr = expr_arena.add(AExpr::Column(col_name.clone()));
                selectors.insert(
                    name.clone(),
                    ExprIR::new(col_expr, OutputName::ColumnLhs(col_name)),
                );
            }
            for expr in exprs {
                selectors.insert(expr.output_name().clone(), expr);
            }
            let selectors = selectors.into_values().collect_vec();
            return build_select_node(phys_input, &selectors, expr_arena, phys_sm, expr_cache);
        },

        IR::Slice { input, offset, len } => {
            let offset = *offset;
            let len = *len as usize;
            let phys_input = lower_ir!(*input)?;
            return Ok(build_slice_node(phys_input, offset, len, phys_sm));
        },

        IR::Filter { input, predicate } => {
            let predicate = predicate.clone();
            let phys_input = lower_ir!(*input)?;
            return build_filter_node(phys_input, predicate, expr_arena, phys_sm, expr_cache);
        },

        IR::DataFrameScan {
            df,
            output_schema: projection,
            schema,
            ..
        } => {
            let schema = schema.clone(); // This is initially the schema of df, but can change with the projection.
            let mut node_kind = PhysNodeKind::InMemorySource { df: df.clone() };

            // Do we need to apply a projection?
            if let Some(projection_schema) = projection {
                if projection_schema.len() != schema.len()
                    || projection_schema
                        .iter_names()
                        .zip(schema.iter_names())
                        .any(|(l, r)| l != r)
                {
                    let phys_input = phys_sm.insert(PhysNode::new(schema, node_kind));
                    node_kind = PhysNodeKind::SimpleProjection {
                        input: phys_input,
                        columns: projection_schema.iter_names_cloned().collect::<Vec<_>>(),
                    };
                }
            }

            node_kind
        },

        IR::Sink { input, payload } => match payload {
            SinkType::Memory => {
                let phys_input = lower_ir!(*input)?;
                PhysNodeKind::InMemorySink { input: phys_input }
            },
            SinkType::File {
                path,
                file_type,
                cloud_options: _,
            } => {
                let path = path.clone();
                let file_type = file_type.clone();

                match file_type {
                    #[cfg(feature = "ipc")]
                    FileType::Ipc(_) => {
                        let phys_input = lower_ir!(*input)?;
                        PhysNodeKind::FileSink {
                            path,
                            file_type,
                            input: phys_input,
                        }
                    },
                    _ => todo!(),
                }
            },
        },

        IR::MapFunction { input, function } => {
            // MergeSorted uses a rechunk hack incompatible with the
            // streaming engine.
            #[cfg(feature = "merge_sorted")]
            if let FunctionIR::MergeSorted { .. } = function {
                todo!()
            }

            let function = function.clone();
            let phys_input = lower_ir!(*input)?;

            match function {
                FunctionIR::RowIndex {
                    name,
                    offset,
                    schema: _,
                } => PhysNodeKind::WithRowIndex {
                    input: phys_input,
                    name,
                    offset,
                },

                function if function.is_streamable() => {
                    let map = Arc::new(move |df| function.evaluate(df));
                    PhysNodeKind::Map {
                        input: phys_input,
                        map,
                    }
                },

                function => {
                    let map = Arc::new(move |df| function.evaluate(df));
                    PhysNodeKind::InMemoryMap {
                        input: phys_input,
                        map,
                    }
                },
            }
        },

        IR::Sort {
            input,
            by_column,
            slice,
            sort_options,
        } => PhysNodeKind::Sort {
            by_column: by_column.clone(),
            slice: *slice,
            sort_options: sort_options.clone(),
            input: lower_ir!(*input)?,
        },

        IR::Union { inputs, options } => {
            let options = *options;
            let inputs = inputs
                .clone() // Needed to borrow ir_arena mutably.
                .into_iter()
                .map(|input| lower_ir!(input))
                .collect::<Result<_, _>>()?;

            let mut node = phys_sm.insert(PhysNode {
                output_schema,
                kind: PhysNodeKind::OrderedUnion { inputs },
            });
            if let Some((offset, length)) = options.slice {
                node = build_slice_node(node, offset, length, phys_sm);
            }
            return Ok(node);
        },

        IR::HConcat {
            inputs,
            schema: _,
            options: _,
        } => {
            let inputs = inputs
                .clone() // Needed to borrow ir_arena mutably.
                .into_iter()
                .map(|input| lower_ir!(input))
                .collect::<Result<_, _>>()?;
            PhysNodeKind::Zip {
                inputs,
                null_extend: true,
            }
        },

        v @ IR::Scan { .. } => {
            let IR::Scan {
                sources: scan_sources,
                file_info,
                hive_parts,
                output_schema: scan_output_schema,
                scan_type,
                mut predicate,
                mut file_options,
            } = v.clone()
            else {
                unreachable!();
            };

            if scan_sources.is_empty() {
                // If there are no sources, just provide an empty in-memory source with the right
                // schema.
                PhysNodeKind::InMemorySource {
                    df: Arc::new(DataFrame::empty_with_schema(output_schema.as_ref())),
                }
            } else {
                #[cfg(feature = "ipc")]
                if matches!(scan_type, FileScan::Ipc { .. }) {
                    // @TODO: All the things the IPC source does not support yet.
                    if hive_parts.is_some()
                        || scan_sources.is_cloud_url()
                        || file_options.allow_missing_columns
                        || file_options.slice.is_some_and(|(offset, _)| offset < 0)
                    {
                        todo!();
                    }
                }

                // Operation ordering:
                // * with_row_index() -> slice() -> filter()

                // Some scans have built-in support for applying these operations in an optimized manner.
                let opt_rewrite_to_nodes: (Option<RowIndex>, Option<(i64, usize)>, Option<ExprIR>) =
                    match &scan_type {
                        #[cfg(feature = "parquet")]
                        FileScan::Parquet { .. } => (None, None, None),
                        #[cfg(feature = "ipc")]
                        FileScan::Ipc { .. } => (None, None, predicate.take()),
                        #[cfg(feature = "csv")]
                        FileScan::Csv { options, .. } => {
                            if options.parse_options.comment_prefix.is_none()
                                && std::env::var("POLARS_DISABLE_EXPERIMENTAL_CSV_SLICE").as_deref()
                                    != Ok("1")
                            {
                                // Note: This relies on `CountLines` being exact.
                                (None, None, predicate.take())
                            } else {
                                // There can be comments in the middle of the file, then `CountLines` won't
                                // return an accurate line count :'(.
                                (
                                    file_options.row_index.take(),
                                    file_options.slice.take(),
                                    predicate.take(),
                                )
                            }
                        },
                        _ => todo!(),
                    };

                let node_kind = PhysNodeKind::FileScan {
                    scan_sources,
                    file_info,
                    hive_parts,
                    output_schema: scan_output_schema,
                    scan_type,
                    predicate,
                    file_options,
                };

                let (row_index, slice, predicate) = opt_rewrite_to_nodes;

                let node_kind = if let Some(ri) = row_index {
                    let mut schema = Arc::unwrap_or_clone(output_schema.clone());

                    let v = schema.shift_remove_index(0).unwrap().0;
                    assert_eq!(v, ri.name);
                    let input = phys_sm.insert(PhysNode::new(Arc::new(schema), node_kind));

                    PhysNodeKind::WithRowIndex {
                        input,
                        name: ri.name,
                        offset: Some(ri.offset),
                    }
                } else {
                    node_kind
                };

                let mut node = phys_sm.insert(PhysNode {
                    output_schema,
                    kind: node_kind,
                });

                if let Some((offset, length)) = slice {
                    node = build_slice_node(node, offset, length, phys_sm);
                }

                if let Some(predicate) = predicate {
                    node = build_filter_node(node, predicate, expr_arena, phys_sm, expr_cache)?;
                }

                return Ok(node);
            }
        },

        #[cfg(feature = "python")]
        IR::PythonScan { .. } => todo!(),

        IR::Cache {
            input,
            id,
            cache_hits: _,
        } => {
            let id = *id;
            if let Some(cached) = cache_nodes.get(&id) {
                return Ok(*cached);
            }

            let phys_input = lower_ir!(*input)?;
            cache_nodes.insert(id, phys_input);
            return Ok(phys_input);
        },

        IR::GroupBy {
            input,
            keys,
            aggs,
            schema: _,
            apply,
            maintain_order,
            options,
        } => {
            if apply.is_some() || *maintain_order {
                todo!()
            }

            #[cfg(feature = "dynamic_group_by")]
            if options.dynamic.is_some() || options.rolling.is_some() {
                todo!()
            }

            let key = keys.clone();
            let mut aggs = aggs.clone();
            let options = options.clone();

            polars_ensure!(!keys.is_empty(), ComputeError: "at least one key is required in a group_by operation");

            // TODO: allow all aggregates.
            let mut input_exprs = key.clone();
            for agg in &aggs {
                match expr_arena.get(agg.node()) {
                    AExpr::Agg(expr) => match expr {
                        IRAggExpr::Min { input, .. }
                        | IRAggExpr::Max { input, .. }
                        | IRAggExpr::Mean(input)
                        | IRAggExpr::Sum(input)
                        | IRAggExpr::Var(input, ..)
                        | IRAggExpr::Std(input, ..) => {
                            if is_elementwise_rec_cached(*input, expr_arena, expr_cache) {
                                input_exprs.push(ExprIR::from_node(*input, expr_arena));
                            } else {
                                todo!()
                            }
                        },
                        _ => todo!(),
                    },
                    AExpr::Len => input_exprs.push(key[0].clone()), // Hack, use the first key column for the length.
                    _ => todo!(),
                }
            }

            let phys_input = lower_ir!(*input)?;
            let (trans_input, trans_exprs) =
                lower_exprs(phys_input, &input_exprs, expr_arena, phys_sm, expr_cache)?;
            let trans_key = trans_exprs[..key.len()].to_vec();
            let trans_aggs = aggs
                .iter_mut()
                .zip(trans_exprs.iter().skip(key.len()))
                .map(|(agg, trans_expr)| {
                    let old_expr = expr_arena.get(agg.node()).clone();
                    let new_expr = old_expr.replace_inputs(&[trans_expr.node()]);
                    ExprIR::new(expr_arena.add(new_expr), agg.output_name_inner().clone())
                })
                .collect();

            let mut node = phys_sm.insert(PhysNode::new(
                output_schema,
                PhysNodeKind::GroupBy {
                    input: trans_input,
                    key: trans_key,
                    aggs: trans_aggs,
                },
            ));

            // TODO: actually limit number of groups instead of computing full
            // result and then slicing.
            if let Some((offset, len)) = options.slice {
                node = build_slice_node(node, offset, len, phys_sm);
            }
            return Ok(node);
        },
        IR::Join {
            input_left,
            input_right,
            schema: _,
            left_on,
            right_on,
            options,
        } => {
            let input_left = *input_left;
            let input_right = *input_right;
            let left_on = left_on.clone();
            let right_on = right_on.clone();
            let args = options.args.clone();
            let phys_left = lower_ir!(input_left)?;
            let phys_right = lower_ir!(input_right)?;
            if args.how.is_equi() && !args.validation.needs_checks() {
                // When lowering the expressions for the keys we need to ensure we keep around the
                // payload columns, otherwise the input nodes can get replaced by input-independent
                // nodes since the lowering code does not see we access any non-literal expressions.
                // So we add dummy expressions before lowering and remove them afterwards.
                let mut aug_left_on = left_on.clone();
                for name in phys_sm[phys_left].output_schema.iter_names() {
                    let col_expr = expr_arena.add(AExpr::Column(name.clone()));
                    aug_left_on.push(ExprIR::new(col_expr, OutputName::ColumnLhs(name.clone())));
                }
                let mut aug_right_on = right_on.clone();
                for name in phys_sm[phys_right].output_schema.iter_names() {
                    let col_expr = expr_arena.add(AExpr::Column(name.clone()));
                    aug_right_on.push(ExprIR::new(col_expr, OutputName::ColumnLhs(name.clone())));
                }
                let (trans_input_left, mut trans_left_on) =
                    lower_exprs(phys_left, &aug_left_on, expr_arena, phys_sm, expr_cache)?;
                let (trans_input_right, mut trans_right_on) =
                    lower_exprs(phys_right, &aug_right_on, expr_arena, phys_sm, expr_cache)?;
                trans_left_on.drain(left_on.len()..);
                trans_right_on.drain(right_on.len()..);

                let mut node = phys_sm.insert(PhysNode::new(
                    output_schema,
                    PhysNodeKind::EquiJoin {
                        input_left: trans_input_left,
                        input_right: trans_input_right,
                        left_on: trans_left_on,
                        right_on: trans_right_on,
                        args: args.clone(),
                    },
                ));
                if let Some((offset, len)) = args.slice {
                    node = build_slice_node(node, offset, len, phys_sm);
                }
                return Ok(node);
            } else {
                PhysNodeKind::InMemoryJoin {
                    input_left: phys_left,
                    input_right: phys_right,
                    left_on,
                    right_on,
                    args,
                }
            }
        },
        IR::Distinct { .. } => todo!(),
        IR::ExtContext { .. } => todo!(),
        IR::Invalid => unreachable!(),
    };

    Ok(phys_sm.insert(PhysNode::new(output_schema, node_kind)))
}

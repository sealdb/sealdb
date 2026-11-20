//! Decoding support for `tidb_decode_plan`.
//!
//! The encoded form is base64 of a snappy-compressed text table of plan
//! operators (TiDB `plancodec`: tab-separated `depth, id, task, ...` rows).
//! Decoding rebuilds the operator tree, renames the numeric physical-plan IDs
//! and renders the aligned table TiDB prints.

use std::collections::HashMap;

use crate::builtin_binary_plan::{base64_decode_std, snappy_decode};

const TREE_BODY: char = '│';
const TREE_MIDDLE_NODE: char = '├';
const TREE_LAST_NODE: char = '└';
const TREE_GAP: char = ' ';
const TREE_NODE_IDENTIFIER: char = '─';

/// TiDB `plancodec.PlanDiscardedEncoded` / decoded text.
const PLAN_DISCARDED_ENCODED: &str = "[discard]";
const PLAN_DISCARDED_DECODED: &str = "(plan discarded because too long)";

/// Decode an encoded plan. Returns `None` when the payload is not a valid
/// encoded plan, in which case TiDB's builtin returns the argument unchanged.
pub fn decode_plan(encoded: &str) -> Option<String> {
    if encoded.is_empty() {
        return Some(String::new());
    }
    if encoded == PLAN_DISCARDED_ENCODED {
        return Some(PLAN_DISCARDED_DECODED.to_string());
    }
    let compressed = base64_decode_std(encoded).ok()?;
    let bytes = snappy_decode(&compressed).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some(build_plan_tree(&text))
}

struct PlanInfo {
    depth: usize,
    fields: Vec<String>,
}

fn build_plan_tree(plan_text: &str) -> String {
    let mut infos: Vec<PlanInfo> = Vec::new();
    for node in plan_text.split('\n') {
        if let Some(info) = decode_plan_info(node) {
            infos.push(info);
        }
    }
    if infos.is_empty() {
        return String::new();
    }

    // Header: the first row's field count decides whether runtime info exists.
    const HEADER_FIELDS: [&str; 8] = [
        "id",
        "task",
        "estRows",
        "operator info",
        "actRows",
        "execution info",
        "memory",
        "disk",
    ];
    let header_len = HEADER_FIELDS.len().min(infos[0].fields.len());
    let mut rows: Vec<PlanInfo> = Vec::with_capacity(infos.len() + 1);
    rows.push(PlanInfo {
        depth: 0,
        fields: HEADER_FIELDS[..header_len]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    });
    rows.extend(infos);

    let depths: Vec<usize> = rows.iter().map(|p| p.depth).collect();

    // Calculate the tree indentation of every row.
    let mut indents: Vec<Vec<char>> = Vec::with_capacity(depths.len());
    for &depth in &depths {
        let mut indent = vec![TREE_GAP; 2 * depth];
        if let Some(len) = indent.len().checked_sub(2) {
            indent[len] = TREE_LAST_NODE;
            indent[len + 1] = TREE_NODE_IDENTIFIER;
        }
        indents.push(indent);
    }
    let mut cache_parent: HashMap<usize, usize> = HashMap::new();
    for i in 1..depths.len() {
        let parent = find_parent_index(&depths, i, &mut cache_parent);
        fill_indent(&depths, &mut indents, parent, i);
    }

    // Align the value of plan fields (the last column is never padded).
    let max_fields = rows.iter().map(|p| p.fields.len()).max().unwrap_or(0);
    for row in &mut rows {
        while row.fields.len() < max_fields {
            row.fields.push(String::new());
        }
    }
    let field_count = rows[0].fields.len().saturating_sub(1);
    for col in 0..field_count {
        let max_len = (0..rows.len())
            .map(|row_idx| plan_field_len(&rows[row_idx], &indents[row_idx], col))
            .max()
            .unwrap_or(0);
        for row_idx in 0..rows.len() {
            let cur = plan_field_len(&rows[row_idx], &indents[row_idx], col);
            if max_len > cur {
                let pad = " ".repeat(max_len - cur);
                rows[row_idx].fields[col].push_str(&pad);
            }
        }
    }

    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push('\t');
        out.extend(indents[i].iter());
        for (j, field) in row.fields.iter().enumerate() {
            if j > 0 {
                out.push('\t');
            }
            out.push_str(field);
        }
    }
    out
}

/// Length used for alignment: the id column includes the tree indentation,
/// other columns use their byte length (Go `len`).
fn plan_field_len(info: &PlanInfo, indent: &[char], col: usize) -> usize {
    if col == 0 {
        info.fields[0].len() + indent.len()
    } else {
        info.fields[col].len()
    }
}

fn find_parent_index(
    depths: &[usize],
    child: usize,
    cache: &mut HashMap<usize, usize>,
) -> usize {
    cache.insert(depths[child], child);
    let Some(parent_depth) = depths[child].checked_sub(1) else {
        return 0;
    };
    if let Some(&parent) = cache.get(&parent_depth) {
        return parent;
    }
    for i in (1..child).rev() {
        if depths[i] == parent_depth {
            cache.insert(depths[i], i);
            return i;
        }
    }
    0
}

fn fill_indent(depths: &[usize], indents: &mut [Vec<char>], parent: usize, child: usize) {
    let depth = depths[child];
    if depth == 0 {
        return;
    }
    let idx = depth * 2 - 2;
    for i in (parent + 1..child).rev() {
        if indents[i][idx] == TREE_LAST_NODE {
            indents[i][idx] = TREE_MIDDLE_NODE;
            break;
        }
        indents[i][idx] = TREE_BODY;
    }
}

fn decode_plan_info(row: &str) -> Option<PlanInfo> {
    let values: Vec<&str> = row.split('\t').collect();
    if values.len() < 2 {
        return None;
    }
    let depth = values[0].parse::<usize>().ok()?;
    let mut fields = Vec::with_capacity(values.len() - 1);
    for (i, value) in values.iter().enumerate() {
        match i {
            0 => {}
            1 => {
                let ids: Vec<&str> = value.split('_').collect();
                if ids.len() != 1 && ids.len() != 2 {
                    return None;
                }
                let plan_id = ids[0].parse::<i64>().ok()?;
                let name = physical_id_to_type_string(plan_id);
                if ids.len() == 1 {
                    fields.push(name);
                } else {
                    fields.push(format!("{name}_{}", ids[1]));
                }
            }
            2 => fields.push(decode_task_type(value)),
            _ => fields.push((*value).to_string()),
        }
    }
    Some(PlanInfo { depth, fields })
}

/// TiDB's task column: `0` (root) or a coprocessor task.
fn decode_task_type(value: &str) -> String {
    let mut parts = value.split('_');
    if parts.next() == Some("0") {
        return "root".to_string();
    }
    match parts.next() {
        // `NormalizePlanNode` omits the store type in the task field.
        None => "cop".to_string(),
        Some(store) => match store.parse::<i64>() {
            Ok(_) => format!("cop[{store}]"),
            Err(_) => "cop".to_string(),
        },
    }
}

/// TiDB `plancodec.PhysicalIDToTypeString` for the operators this engine's
/// plan encoding can contain (v8.5 IDs, which are stable for compatibility).
fn physical_id_to_type_string(id: i64) -> String {
    let name = match id {
        1 => "Selection",
        2 => "Set",
        3 => "Projection",
        4 => "Aggregation",
        5 => "StreamAgg",
        6 => "HashAgg",
        7 => "Show",
        8 => "Join",
        9 => "Union",
        10 => "TableScan",
        11 => "MemTableScan",
        12 => "UnionScan",
        13 => "IndexScan",
        14 => "Sort",
        15 => "TopN",
        16 => "Limit",
        17 => "HashJoin",
        18 => "MergeJoin",
        19 => "IndexJoin",
        20 => "IndexMergeJoin",
        21 => "IndexHashJoin",
        22 => "Apply",
        23 => "MaxOneRow",
        24 => "Exists",
        25 => "TableDual",
        26 => "SelectLock",
        27 => "Insert",
        28 => "Update",
        29 => "Delete",
        30 => "IndexLookUp",
        31 => "TableReader",
        32 => "IndexReader",
        33 => "Window",
        34 => "TiKVSingleGather",
        35 => "IndexMerge",
        36 => "Point_Get",
        37 => "ShowDDLJobs",
        38 => "Batch_Point_Get",
        39 => "ClusterMemTableReader",
        40 => "DataSource",
        41 => "LoadData",
        42 => "TableSample",
        43 => "TableFullScan",
        44 => "TableRangeScan",
        45 => "TableRowIDScan",
        46 => "IndexFullScan",
        47 => "IndexRangeScan",
        48 => "ExchangeReceiver",
        49 => "ExchangeSender",
        50 => "CTEFullScan",
        51 => "CTE",
        52 => "CTETable",
        other => return format!("UnknownPlanID{other}"),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::decode_plan;

    const PLAN_WITHOUT_RUNTIME_INFO: &str = "7APIMAk1XzEzCTAJMQlmdW5jczpjb3VudCgxKQoxCTE3XzE0CTAJMAlpbm5lciBqb2luLCBpAQyQOlRhYmxlUmVhZGVyXzIxLCBlcXVhbDpbZXEoQ29sdW1uIzEsIA0KCDkpIBkXADIVFywxMCldCjIJMzFfMTgFZXhkYXRhOlNlbGVjdGlvbl8xNwozCTFfMTcJMQkwCWx0HVlATlVMTCksIG5vdChpc251bGwVHAApUhcAUDIpKQo0CTEwXzE2CTEJMTAwMDAJdAHB2Dp0MSwgcmFuZ2U6Wy1pbmYsK2luZl0sIGtlZXAgb3JkZXI6ZmFsc2UsIHN0YXRzOnBzZXVkbwoFtgAyAZcEMAk6tgAEMjAFtgQyMDq2AAg5LCBmtgAAMFa3AAA5FbcAOT63AAAyzrcA";

    #[test]
    fn decodes_plan_without_runtime_info() {
        let plan = decode_plan(PLAN_WITHOUT_RUNTIME_INFO).expect("decodable");
        assert!(plan.starts_with("\tid"));
        assert!(plan.contains("\tStreamAgg_13"));
        assert!(plan.contains("\t└─HashJoin_14"));
        assert!(plan.contains("│ └─Selection_17"));
        assert!(plan.contains("\tfuncs:count(1)"));
        assert!(plan.contains("\ttable:t1, range:[-inf,+inf], keep order:false, stats:pseudo"));
    }

    #[test]
    fn invalid_payload_is_not_decodable() {
        assert_eq!(decode_plan("xxx"), None);
        assert_eq!(decode_plan(""), Some(String::new()));
    }
}

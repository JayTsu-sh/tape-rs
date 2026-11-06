//! 设备库的软反亲和；这里只计算目标，实际接管仍经 Coordinator/Raft/PR。
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub(crate) struct Library {
    pub candidates: BTreeSet<u64>,
    pub owner: Option<u64>,
}

/// 优先降低节点负载平方和，其次保留健康 owner；输入按库/节点排序以稳定决策。
pub(crate) fn spread(libraries: &BTreeMap<String, Library>) -> BTreeMap<String, u64> {
    let libraries: Vec<_> = libraries
        .iter()
        .filter(|(_, l)| !l.candidates.is_empty())
        .collect();
    let nodes: Vec<_> = libraries
        .iter()
        .flat_map(|(_, l)| l.candidates.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let count = libraries.len();
    let first_node = 1 + count;
    let sink = first_node + nodes.len();
    let mut graph = vec![Vec::new(); sink + 1];
    // 一次移动最多节约 count；负载成本权重大于它，保证先分散、再少移动。
    let weight = count as i64 + 1;
    for (i, (_, library)) in libraries.iter().enumerate() {
        connect(&mut graph, 0, 1 + i, 0);
        for (j, node) in nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| library.candidates.contains(n))
        {
            connect(
                &mut graph,
                1 + i,
                first_node + j,
                i64::from(library.owner != Some(*node)),
            );
        }
    }
    for j in 0..nodes.len() {
        for slot in 0..count {
            // 0,2,4,... 累加为 load*(load-1)，与负载平方只差固定总库数。
            connect(&mut graph, first_node + j, sink, 2 * slot as i64 * weight);
        }
    }
    while augment(&mut graph, sink) {}
    libraries
        .iter()
        .enumerate()
        .filter_map(|(i, (id, _))| {
            graph[1 + i]
                .iter()
                .find(|e| !e.available && (first_node..sink).contains(&e.to))
                .map(|e| ((*id).clone(), nodes[e.to - first_node]))
        })
        .collect()
}

#[derive(Clone)]
struct Edge {
    to: usize,
    reverse: usize,
    cost: i64,
    available: bool,
}

fn connect(graph: &mut [Vec<Edge>], from: usize, to: usize, cost: i64) {
    let reverse = graph[to].len();
    let forward = graph[from].len();
    graph[from].push(Edge {
        to,
        reverse,
        cost,
        available: true,
    });
    graph[to].push(Edge {
        to: from,
        reverse: forward,
        cost: -cost,
        available: false,
    });
}

// 最短增广路允许重新安排受限库，避免逐库贪心把唯一可用节点占满。
fn augment(graph: &mut [Vec<Edge>], sink: usize) -> bool {
    let mut distance = vec![i64::MAX; graph.len()];
    let mut previous = vec![None; graph.len()];
    let mut queued = vec![false; graph.len()];
    let mut queue = VecDeque::from([0]);
    distance[0] = 0;
    queued[0] = true;
    while let Some(from) = queue.pop_front() {
        queued[from] = false;
        for (index, edge) in graph[from].iter().enumerate() {
            if edge.available && distance[edge.to] > distance[from] + edge.cost {
                distance[edge.to] = distance[from] + edge.cost;
                previous[edge.to] = Some((from, index));
                if !queued[edge.to] {
                    queue.push_back(edge.to);
                    queued[edge.to] = true;
                }
            }
        }
    }
    let mut at = sink;
    if previous[at].is_none() {
        return false;
    }
    while at != 0 {
        let (from, index) = previous[at].unwrap();
        let edge = &mut graph[from][index];
        edge.available = false;
        let reverse = edge.reverse;
        graph[at][reverse].available = true;
        at = from;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn libraries(rows: &[(&str, &[u64], Option<u64>)]) -> BTreeMap<String, Library> {
        rows.iter()
            .map(|(id, nodes, owner)| {
                (
                    (*id).into(),
                    Library {
                        candidates: nodes.iter().copied().collect(),
                        owner: *owner,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn two_libraries_spread_and_keep_one_healthy_owner() {
        let plan = spread(&libraries(&[
            ("a", &[1, 2, 3], Some(2)),
            ("b", &[1, 2, 3], Some(2)),
        ]));
        assert_ne!(plan["a"], plan["b"]);
        assert_eq!(plan.values().filter(|node| **node == 2).count(), 1);
    }

    #[test]
    fn a_balanced_healthy_assignment_does_not_move() {
        let plan = spread(&libraries(&[
            ("a", &[1, 2, 3], Some(3)),
            ("b", &[1, 2, 3], Some(2)),
        ]));
        assert_eq!(plan, BTreeMap::from([("a".into(), 3), ("b".into(), 2)]));
    }

    #[test]
    fn failed_paths_allow_multiple_libraries_on_the_only_healthy_node() {
        let plan = spread(&libraries(&[
            ("a", &[2], Some(1)),
            ("b", &[2], Some(2)),
            ("offline", &[], None),
        ]));
        assert_eq!(plan, BTreeMap::from([("a".into(), 2), ("b".into(), 2)]));
    }

    #[test]
    fn constraints_are_matched_without_greedy_collisions() {
        let plan = spread(&libraries(&[
            ("a", &[1, 2], Some(1)),
            ("b", &[2, 3], Some(2)),
            ("c", &[1, 2], Some(1)),
        ]));
        assert_eq!(plan.values().copied().collect::<BTreeSet<_>>().len(), 3);
        assert_eq!(plan["b"], 3);
    }

    #[test]
    fn more_libraries_than_nodes_are_balanced_with_minimal_moves() {
        let rows: Vec<_> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .map(|id| (id, &[1, 2][..], Some(1)))
            .collect();
        let plan = spread(&libraries(&rows));
        let moved = plan.values().filter(|node| **node == 2).count();
        assert_eq!(moved, 2);
    }

    #[test]
    fn restoring_a_candidate_spreads_once_then_stays_put() {
        let mut input = libraries(&[("a", &[1, 2], Some(1)), ("b", &[1, 2], Some(1))]);
        let first = spread(&input);
        assert_ne!(first["a"], first["b"]);
        for (id, library) in &mut input {
            library.owner = Some(first[id]);
        }
        assert_eq!(spread(&input), first);
    }

    #[test]
    fn serial_and_node_order_make_the_same_input_repeatable() {
        let input = libraries(&[("b", &[9, 4, 2], None), ("a", &[4, 9, 2], None)]);
        let plan = spread(&input);
        assert_eq!(plan.values().copied().collect::<BTreeSet<_>>().len(), 2);
        for _ in 0..10 {
            assert_eq!(spread(&input), plan);
        }
    }

    #[test]
    fn every_small_candidate_graph_matches_the_exhaustive_optimum() {
        for mask in 0_u16..512 {
            let mut input = BTreeMap::new();
            for (i, id) in ["a", "b", "c"].iter().enumerate() {
                input.insert(
                    (*id).into(),
                    Library {
                        candidates: (1..=3)
                            .filter(|node| mask & (1 << (i * 3 + *node as usize - 1)) != 0)
                            .collect(),
                        owner: Some([1, 2, 1][i]),
                    },
                );
            }
            let weight = input.values().filter(|l| !l.candidates.is_empty()).count() + 1;
            let score = |plan: &BTreeMap<String, u64>| {
                let mut load = [0_usize; 3];
                let mut moves = 0;
                for (id, node) in plan {
                    load[*node as usize - 1] += 1;
                    moves += usize::from(input[id].owner != Some(*node));
                }
                load.iter().map(|n| n * n).sum::<usize>() * weight + moves
            };
            let plan = spread(&input);
            let mut best = usize::MAX;
            for a in 0..=3 {
                for b in 0..=3 {
                    for c in 0..=3 {
                        let candidate: BTreeMap<_, _> = ["a", "b", "c"]
                            .into_iter()
                            .zip([a, b, c])
                            .filter(|(_, node)| *node != 0)
                            .map(|(id, node)| (id.into(), node))
                            .collect();
                        if input.iter().all(|(id, l)| match candidate.get(id) {
                            Some(node) => l.candidates.contains(node),
                            None => l.candidates.is_empty(),
                        }) {
                            best = best.min(score(&candidate));
                        }
                    }
                }
            }
            assert_eq!(score(&plan), best, "candidate graph {mask}");
        }
    }
}

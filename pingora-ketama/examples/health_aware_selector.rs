use log::info;
use pingora_ketama::{Bucket, Continuum};
use std::collections::HashMap;
use std::net::SocketAddr;

// A repository for node healthiness, emulating a health checker.
struct NodeHealthRepository {
    nodes: HashMap<SocketAddr, bool>,
}

impl NodeHealthRepository {
    fn new() -> Self {
        NodeHealthRepository {
            nodes: HashMap::new(),
        }
    }

    fn set_node_health(&mut self, node: SocketAddr, is_healthy: bool) {
        self.nodes.insert(node, is_healthy);
    }

    fn node_is_healthy(&self, node: &SocketAddr) -> bool {
        self.nodes.get(node).cloned().unwrap_or(false)
    }
}

// A health-aware node selector, which relies on the above health repository.
struct HealthAwareNodeSelector<'a> {
    ring: Continuum,
    max_tries: usize,
    node_health_repo: &'a NodeHealthRepository,
}

impl<'a> HealthAwareNodeSelector<'a> {
    fn new(r: Continuum, tries: usize, nhr: &NodeHealthRepository) -> HealthAwareNodeSelector {
        HealthAwareNodeSelector {
            ring: r,
            max_tries: tries,
            node_health_repo: nhr,
        }
    }

    // Try to select a node within <max_tries> attempts.
    fn try_select(&self, key: &str) -> Option<SocketAddr> {
        let node_iter = self.ring.node_iter(key.as_bytes());

        for (tries, node) in node_iter.enumerate() {
            if tries >= self.max_tries {
                break;
            }

            if self.node_health_repo.node_is_healthy(node) {
                return Some(*node);
            }
        }

        None
    }
}

// RUST_LOG=INFO cargo run --example health_aware_selector
fn main() {
    env_logger::init();

    // Set up some nodes.
    let buckets: Vec<_> = (1..=10)
        .map(|i| Bucket::new(format!("127.0.0.{i}:6443").parse().unwrap(), 1))
        .collect();

    // Mark the 1-5th nodes healthy, the 6-10th nodes unhealthy.
    let mut health_repo = NodeHealthRepository::new();
    (1..=10)
        .map(|i| (i, format!("127.0.0.{i}:6443").parse().unwrap()))
        .for_each(|(i, n)| {
            health_repo.set_node_health(n, i < 6);
        });

    // Create a health-aware selector with up to 3 tries.
    let health_aware_selector =
        HealthAwareNodeSelector::new(Continuum::new(&buckets), 3, &health_repo);

    // Let's try the selector on a few keys.
    for i in 0..5 {
        let key = format!("key_{i}");
        match health_aware_selector.try_select(&key) {
            Some(node) => {
                info!("{key}: {}:{}", node.ip(), node.port());
            }
            None => {
                info!("{key}: no healthy node found!");
            }
        }
    }
}

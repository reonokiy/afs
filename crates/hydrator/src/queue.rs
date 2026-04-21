use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::Path;
use std::time::Instant;

/// Priority levels for blob hydration.
pub const PRIORITY_EXPLICIT_READ: u32 = 1000;
pub const PRIORITY_SIBLING: u32 = 800;
pub const PRIORITY_BOOTSTRAP: u32 = 700;
pub const PRIORITY_LIKELY_TEXT: u32 = 500;
pub const PRIORITY_NEARBY_CODE: u32 = 400;
pub const PRIORITY_BINARY: u32 = 100;

/// A task in the hydration queue.
#[derive(Debug, Clone)]
pub struct HydrationTask {
    pub oid: String,
    pub path: String,
    pub priority: u32,
    pub reason: &'static str,
    pub enqueued_at: Instant,
}

impl Eq for HydrationTask {}

impl PartialEq for HydrationTask {
    fn eq(&self, other: &Self) -> bool {
        self.oid == other.oid
    }
}

impl Ord for HydrationTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first; on tie, earlier enqueue time first
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.enqueued_at.cmp(&self.enqueued_at))
    }
}

impl PartialOrd for HydrationTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Classify a file path into a hydration priority.
pub fn classify_priority(path: &str) -> u32 {
    let base = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    match base {
        "README" | "README.md" | "LICENSE" | "Makefile" | ".gitignore" => PRIORITY_BOOTSTRAP,
        "go.mod" | "go.sum" | "Cargo.toml" | "Cargo.lock" | "package.json"
        | "pnpm-lock.yaml" | "pyproject.toml" | "requirements.txt" => PRIORITY_BOOTSTRAP,
        _ => match ext {
            "go" | "rs" | "zig" | "py" | "ts" | "tsx" | "js" | "jsx" | "java" | "c" | "cc"
            | "cpp" | "h" | "hpp" | "json" | "yaml" | "yml" | "toml" | "md" => {
                PRIORITY_LIKELY_TEXT
            }
            "png" | "jpg" | "jpeg" | "gif" | "zip" | "pdf" | "tar" | "gz" | "mp4" | "mov"
            | "avi" | "wasm" => PRIORITY_BINARY,
            _ => PRIORITY_NEARBY_CODE,
        },
    }
}

/// The priority queue for hydration tasks.
#[derive(Default)]
pub struct HydrationQueue {
    heap: BinaryHeap<HydrationTask>,
}

impl HydrationQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, task: HydrationTask) {
        self.heap.push(task);
    }

    pub fn pop(&mut self) -> Option<HydrationTask> {
        self.heap.pop()
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_ordering() {
        let now = Instant::now();
        let mut q = HydrationQueue::new();

        q.push(HydrationTask {
            oid: "low".into(),
            path: "image.png".into(),
            priority: PRIORITY_BINARY,
            reason: "prefetch",
            enqueued_at: now,
        });
        q.push(HydrationTask {
            oid: "high".into(),
            path: "src/main.rs".into(),
            priority: PRIORITY_EXPLICIT_READ,
            reason: "explicit read",
            enqueued_at: now,
        });
        q.push(HydrationTask {
            oid: "mid".into(),
            path: "Cargo.toml".into(),
            priority: PRIORITY_BOOTSTRAP,
            reason: "prefetch",
            enqueued_at: now,
        });

        assert_eq!(q.pop().unwrap().oid, "high");
        assert_eq!(q.pop().unwrap().oid, "mid");
        assert_eq!(q.pop().unwrap().oid, "low");
    }

    #[test]
    fn classify() {
        assert_eq!(classify_priority("go.mod"), PRIORITY_BOOTSTRAP);
        assert_eq!(classify_priority("src/main.rs"), PRIORITY_LIKELY_TEXT);
        assert_eq!(classify_priority("assets/logo.png"), PRIORITY_BINARY);
        assert_eq!(classify_priority("data/config.txt"), PRIORITY_NEARBY_CODE);
    }
}

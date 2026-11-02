use std::collections::HashSet;

use super::CommitFile;
use super::File;

fn tree_path_for_name(name: &str) -> &str {
    name.split_once(" -> ").map_or(name, |(_, new)| new)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::model::{FileChangeStatus, FileStatus};

    #[test]
    fn file_tree_groups_rename_under_new_path_and_uses_leaf_arrow_label() {
        let files = vec![File {
            name: "src/views/openai_oauth_flow.rs -> src/views/provider_oauth_flow.rs".to_string(),
            display_name: "src/views/openai_oauth_flow.rs -> src/views/provider_oauth_flow.rs"
                .to_string(),
            status: FileStatus::Renamed,
            has_staged_changes: true,
            has_unstaged_changes: false,
            tracked: true,
            added: false,
            deleted: false,
            has_merge_conflicts: false,
            short_status: "R ".to_string(),
            hunk_count: 0,
            additions: 0,
            deletions: 0,
        }];

        let nodes = build_file_tree(&files, &HashSet::new());

        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "src/views");
        assert_eq!(nodes[0].path, "src/views");
        assert!(nodes[0].is_dir);
        assert_eq!(
            nodes[1].name,
            "openai_oauth_flow.rs -> provider_oauth_flow.rs"
        );
        assert_eq!(nodes[1].path, "src/views/provider_oauth_flow.rs");
        assert_eq!(nodes[1].file_index, Some(0));
    }

    #[test]
    fn commit_file_tree_groups_rename_under_new_path() {
        let files = vec![CommitFile {
            name: "src/views/openai_oauth_flow.rs -> src/views/provider_oauth_flow.rs".to_string(),
            status: FileChangeStatus::Renamed,
            hunk_count: 0,
            additions: 0,
            deletions: 0,
        }];

        let nodes = build_commit_file_tree(&files, &HashSet::new());

        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "src/views");
        assert_eq!(
            nodes[1].name,
            "openai_oauth_flow.rs -> provider_oauth_flow.rs"
        );
        assert_eq!(nodes[1].path, "src/views/provider_oauth_flow.rs");
        assert_eq!(nodes[1].file_index, Some(0));
    }

    #[test]
    fn test_tree_navigation() {
        struct TestNode {
            depth: usize,
        }

        impl NavigableTreeNode for TestNode {
            fn depth(&self) -> usize {
                self.depth
            }
        }

        // Tree structure:
        // 0: root (depth 0, dir)
        // 1:   child1 (depth 1, dir)
        // 2:     grandchild1 (depth 2, file)
        // 3:     grandchild2 (depth 2, file)
        // 4:   child2 (depth 1, file)
        let nodes = vec![
            TestNode { depth: 0 },
            TestNode { depth: 1 },
            TestNode { depth: 2 },
            TestNode { depth: 2 },
            TestNode { depth: 1 },
        ];

        for idx in [nodes.len(), usize::MAX] {
            assert_eq!(find_parent_idx(&nodes, idx), None);
            assert_eq!(find_first_child_idx(&nodes, idx), None);
            assert_eq!(find_prev_sibling_idx(&nodes, idx), None);
            assert_eq!(find_next_sibling_idx(&nodes, idx), None);
        }

        // Parent navigation
        assert_eq!(find_parent_idx(&nodes, 2), Some(1));
        assert_eq!(find_parent_idx(&nodes, 1), Some(0));
        assert_eq!(find_parent_idx(&nodes, 0), None);
        assert_eq!(find_parent_idx(&nodes, 4), Some(0));

        // First child navigation
        assert_eq!(find_first_child_idx(&nodes, 0), Some(1));
        assert_eq!(find_first_child_idx(&nodes, 1), Some(2));
        assert_eq!(find_first_child_idx(&nodes, 2), None); // Not a directory / no child

        // Sibling navigation
        assert_eq!(find_next_sibling_idx(&nodes, 2), Some(3)); // grandchild1 -> grandchild2
        assert_eq!(find_next_sibling_idx(&nodes, 3), None); // grandchild2 is last child
        assert_eq!(find_next_sibling_idx(&nodes, 1), Some(4)); // child1 -> child2 (skipping grandchildren!)
        assert_eq!(find_prev_sibling_idx(&nodes, 3), Some(2)); // grandchild2 -> grandchild1
        assert_eq!(find_prev_sibling_idx(&nodes, 4), Some(1)); // child2 -> child1 (skipping grandchildren!)
    }
}

fn rename_leaf_name(name: &str) -> Option<String> {
    let (old, new) = name.split_once(" -> ")?;
    let old_leaf = old.rsplit('/').next().unwrap_or(old);
    let new_leaf = new.rsplit('/').next().unwrap_or(new);
    Some(format!("{} -> {}", old_leaf, new_leaf))
}

/// A node in a flattened file tree for display.
#[derive(Debug, Clone)]
pub struct FileTreeNode {
    /// Indentation depth (0 = root level).
    pub depth: usize,
    /// Display name (just the directory or file name, not the full path).
    pub name: String,
    /// Full directory path (e.g. "src/gui") for directories, used for collapse tracking.
    pub path: String,
    /// If this is a file node, the index into the source file slice.
    pub file_index: Option<usize>,
    pub is_dir: bool,
    /// For directory nodes: indices into the source file slice of all descendants.
    pub child_file_indices: Vec<usize>,
}

/// Working-tree and commit-file trees have the same presentation shape.  Keep
/// one node type so navigation/rendering utilities do not need parallel models.
pub type CommitFileTreeNode = FileTreeNode;

trait TreeFile {
    fn tree_name(&self) -> &str;
}

impl TreeFile for File {
    fn tree_name(&self) -> &str {
        &self.name
    }
}

impl TreeFile for CommitFile {
    fn tree_name(&self) -> &str {
        &self.name
    }
}

/// Build a flat list of tree nodes from the working-tree file list.
/// `collapsed_dirs` controls which directories are collapsed (hidden children).
pub fn build_file_tree(files: &[File], collapsed_dirs: &HashSet<String>) -> Vec<FileTreeNode> {
    build_tree(files, collapsed_dirs)
}

/// Build a flat list of tree nodes from a commit's file list.
pub fn build_commit_file_tree(
    files: &[CommitFile],
    collapsed_dirs: &HashSet<String>,
) -> Vec<CommitFileTreeNode> {
    build_tree(files, collapsed_dirs)
}

/// Shared tree construction for working-tree and commit files.  This remains
/// monomorphized for each file type, so sharing the source does not add dynamic
/// dispatch to this render-sensitive path.
fn build_tree<T: TreeFile>(files: &[T], collapsed_dirs: &HashSet<String>) -> Vec<FileTreeNode> {
    if files.is_empty() {
        return Vec::new();
    }

    // Collect (path_parts, file_index) and sort with directories before files.
    let mut entries: Vec<(Vec<&str>, usize)> = files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let parts: Vec<&str> = tree_path_for_name(file.tree_name()).split('/').collect();
            (parts, i)
        })
        .collect();
    entries.sort_by(|a, b| sort_dirs_first(&a.0, &b.0));

    // First pass: collect descendant file indices per directory path.
    let mut dir_children: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (parts, file_idx) in &entries {
        for depth in 0..parts.len().saturating_sub(1) {
            let dir_path = parts[..=depth].join("/");
            dir_children.entry(dir_path).or_default().push(*file_idx);
        }
    }

    let mut nodes = vec![FileTreeNode {
        depth: 0,
        name: ".".to_string(),
        path: ".".to_string(),
        file_index: None,
        is_dir: true,
        child_file_indices: (0..files.len()).collect(),
    }];

    if collapsed_dirs.contains(".") {
        return nodes;
    }

    let mut last_dirs: Vec<String> = Vec::new();

    for (parts, file_idx) in &entries {
        let dir_parts = &parts[..parts.len() - 1];
        let file_name = rename_leaf_name(files[*file_idx].tree_name())
            .unwrap_or_else(|| parts[parts.len() - 1].to_string());

        let hidden = (0..dir_parts.len()).any(|depth| {
            let ancestor_path = parts[..=depth].join("/");
            collapsed_dirs.contains(&ancestor_path)
        });

        let common_prefix = last_dirs
            .iter()
            .zip(dir_parts.iter())
            .take_while(|(a, b)| a.as_str() == **b)
            .count();

        for (depth, dir) in dir_parts.iter().enumerate().skip(common_prefix) {
            let dir_path = parts[..=depth].join("/");
            let dir_hidden = (0..depth).any(|ancestor_depth| {
                let ancestor = parts[..=ancestor_depth].join("/");
                collapsed_dirs.contains(&ancestor)
            });

            if !dir_hidden {
                nodes.push(FileTreeNode {
                    depth: depth + 1, // +1 for root node
                    name: dir.to_string(),
                    path: dir_path.clone(),
                    file_index: None,
                    is_dir: true,
                    child_file_indices: dir_children.get(&dir_path).cloned().unwrap_or_default(),
                });
            }

            if collapsed_dirs.contains(&dir_path) {
                break;
            }
        }

        if !hidden {
            nodes.push(FileTreeNode {
                depth: dir_parts.len() + 1, // +1 for root node
                name: file_name,
                path: parts.join("/"),
                file_index: Some(*file_idx),
                is_dir: false,
                child_file_indices: Vec::new(),
            });
        }

        last_dirs = dir_parts.iter().map(|part| part.to_string()).collect();
    }

    compress_single_child_dirs(&mut nodes);

    // Match lazygit: hide a synthetic root that only has one direct child.
    if nodes.first().is_some_and(|node| node.path == ".") {
        let direct_children = nodes[1..].iter().filter(|node| node.depth == 1).count();
        if direct_children == 1 {
            nodes.remove(0);
            for node in &mut nodes {
                node.depth -= 1;
            }
        }
    }

    nodes
}

/// Compress single-child directory chains into combined path nodes.
/// e.g. `apps` → `nextjs` → `src` becomes `apps/nextjs/src` as one node.
fn compress_single_child_dirs(nodes: &mut Vec<FileTreeNode>) {
    let mut i = 0;
    while i < nodes.len() {
        if !nodes[i].is_dir {
            i += 1;
            continue;
        }

        let depth = nodes[i].depth;
        if i + 1 < nodes.len() && nodes[i + 1].is_dir && nodes[i + 1].depth == depth + 1 {
            let has_sibling = (i + 2..nodes.len())
                .take_while(|&j| nodes[j].depth > depth)
                .any(|j| nodes[j].depth == depth + 1);

            if !has_sibling {
                let child = nodes.remove(i + 1);
                if nodes[i].name == "." {
                    nodes[i].name = child.name;
                } else {
                    nodes[i].name = format!("{}/{}", nodes[i].name, child.name);
                }
                nodes[i].path = child.path;
                nodes[i].child_file_indices = child.child_file_indices;

                // Pull all descendants up one level after merging the directory.
                let mut j = i + 1;
                while j < nodes.len() && nodes[j].depth > depth {
                    nodes[j].depth -= 1;
                    j += 1;
                }
                continue; // re-check this node for a longer single-child chain
            }
        }

        i += 1;
    }
}

/// Sort path parts so directories appear before files at each level, then
/// alphabetically within each group.
fn sort_dirs_first(a: &[&str], b: &[&str]) -> std::cmp::Ordering {
    for i in 0..a.len().min(b.len()) {
        if a[i] != b[i] {
            let a_is_dir = i < a.len() - 1;
            let b_is_dir = i < b.len() - 1;
            if a_is_dir != b_is_dir {
                return if a_is_dir {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            return a[i].cmp(b[i]);
        }
    }
    b.len().cmp(&a.len())
}

pub trait NavigableTreeNode {
    fn depth(&self) -> usize;
}

impl NavigableTreeNode for FileTreeNode {
    fn depth(&self) -> usize {
        self.depth
    }
}

pub fn find_parent_idx<T: NavigableTreeNode>(nodes: &[T], current_idx: usize) -> Option<usize> {
    if current_idx == 0 || nodes.is_empty() || current_idx >= nodes.len() {
        return None;
    }
    let current_depth = nodes[current_idx].depth();
    if current_depth == 0 {
        return None;
    }
    let target_depth = current_depth - 1;
    for idx in (0..current_idx).rev() {
        if nodes[idx].depth() == target_depth {
            return Some(idx);
        }
    }
    None
}

pub fn find_first_child_idx<T: NavigableTreeNode>(
    nodes: &[T],
    current_idx: usize,
) -> Option<usize> {
    if current_idx >= nodes.len() || current_idx + 1 >= nodes.len() {
        return None;
    }
    let current_depth = nodes[current_idx].depth();
    if nodes[current_idx + 1].depth() == current_depth + 1 {
        return Some(current_idx + 1);
    }
    None
}

pub fn find_prev_sibling_idx<T: NavigableTreeNode>(
    nodes: &[T],
    current_idx: usize,
) -> Option<usize> {
    if current_idx == 0 || nodes.is_empty() || current_idx >= nodes.len() {
        return None;
    }
    let current_depth = nodes[current_idx].depth();
    for idx in (0..current_idx).rev() {
        let depth = nodes[idx].depth();
        if depth < current_depth {
            break;
        }
        if depth == current_depth {
            return Some(idx);
        }
    }
    None
}

pub fn find_next_sibling_idx<T: NavigableTreeNode>(
    nodes: &[T],
    current_idx: usize,
) -> Option<usize> {
    if current_idx >= nodes.len() || current_idx + 1 >= nodes.len() {
        return None;
    }
    let current_depth = nodes[current_idx].depth();
    for idx in (current_idx + 1)..nodes.len() {
        let depth = nodes[idx].depth();
        if depth < current_depth {
            break;
        }
        if depth == current_depth {
            return Some(idx);
        }
    }
    None
}

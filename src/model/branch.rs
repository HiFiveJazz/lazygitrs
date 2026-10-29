#[derive(Debug, Clone)]
pub struct Branch {
    pub name: String,
    pub hash: String,
    pub recency: String,
    pub commit_time: i64,
    pub pushables: String,
    pub pullables: String,
    pub upstream: Option<String>,
    pub head: bool,
}

pub fn sort_branches_for_display(branches: &mut Vec<Branch>) {
    branches.sort_by(|a, b| {
        b.commit_time
            .cmp(&a.commit_time)
            .then_with(|| a.name.cmp(&b.name))
    });

    if let Some(index) = branches.iter().position(|branch| branch.head) {
        if index != 0 {
            let head = branches.remove(index);
            branches.insert(0, head);
        }
    }
}

impl Branch {
    pub fn is_tracking(&self) -> bool {
        self.upstream.is_some()
    }

    pub fn ahead_behind(&self) -> Option<(usize, usize)> {
        let ahead = self.pushables.parse::<usize>().unwrap_or(0);
        let behind = self.pullables.parse::<usize>().unwrap_or(0);
        if ahead > 0 || behind > 0 {
            Some((ahead, behind))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sort_branches_for_display, Branch};

    fn branch(name: &str, commit_time: i64, head: bool) -> Branch {
        Branch {
            name: name.to_string(),
            hash: String::new(),
            recency: String::new(),
            commit_time,
            pushables: String::new(),
            pullables: String::new(),
            upstream: None,
            head,
        }
    }

    #[test]
    fn display_sort_moves_head_and_rest_together() {
        let mut branches = vec![
            branch("older", 100, false),
            branch("head", 200, true),
            branch("newest", 300, false),
            branch("alpha", 150, false),
            branch("beta", 150, false),
        ];

        sort_branches_for_display(&mut branches);

        let names: Vec<_> = branches.iter().map(|branch| branch.name.as_str()).collect();
        assert_eq!(names, vec!["head", "newest", "alpha", "beta", "older"]);
    }
}

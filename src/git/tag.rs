use std::collections::HashMap;

use anyhow::Result;

use super::GitCommands;
use crate::model::Tag;

impl GitCommands {
    pub fn load_tags(&self) -> Result<Vec<Tag>> {
        // Remote tag presence is intentionally not queried during normal model
        // loading. A network `ls-remote` made startup latency depend on the
        // remote; after a real fetch/pull we preserve/merge remote state via
        // `load_tags_after_remote_update()` instead.
        self.load_local_tags()
    }

    /// Refresh tags after a successful remote update without querying every remote.
    ///
    /// Existing tags retain their previously known remote state. Tags that appeared
    /// during the remote operation are assumed to have been fetched from a remote.
    pub fn load_tags_after_remote_update(
        &self,
        previous_remote_state: &HashMap<String, bool>,
    ) -> Result<Vec<Tag>> {
        let mut tags = self.load_local_tags()?;
        apply_remote_update_state(&mut tags, previous_remote_state);
        Ok(tags)
    }

    /// Load local tag refs without querying any remotes.
    fn load_local_tags(&self) -> Result<Vec<Tag>> {
        if let Some(repo) = &self.read_repo
            && let Ok(tags) = repo.tags()
        {
            return Ok(tags
                .into_iter()
                .map(|tag| Tag {
                    name: tag.name,
                    hash: tag.hash,
                    message: tag.message,
                    on_remote: false,
                })
                .collect());
        }

        self.load_local_tags_cli()
    }

    fn load_local_tags_cli(&self) -> Result<Vec<Tag>> {
        // Peel annotated tags to the commit they point at.
        let format = "%(refname:short)|%(if)%(*objectname)%(then)%(*objectname:short)%(else)%(objectname:short)%(end)|%(subject)";
        let result = self
            .git()
            .args(&[
                "for-each-ref",
                "--sort=-creatordate",
                &format!("--format={}", format),
                "refs/tags/",
            ])
            .run()?;

        if !result.success {
            return Ok(Vec::new());
        }

        let tags = result
            .stdout
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(3, '|').collect();
                if parts.len() >= 2 {
                    Some(Tag {
                        name: parts[0].to_string(),
                        hash: parts[1].to_string(),
                        message: parts.get(2).unwrap_or(&"").to_string(),
                        on_remote: false,
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(tags)
    }

    pub fn create_tag(&self, name: &str, message: &str) -> Result<()> {
        if message.is_empty() {
            self.git().args(&["tag", name]).run_expecting_success()?;
        } else {
            self.git()
                .args(&["tag", "-a", name, "-m", message])
                .run_expecting_success()?;
        }
        Ok(())
    }

    pub fn delete_tag(&self, name: &str) -> Result<()> {
        self.git()
            .args(&["tag", "-d", name])
            .run_expecting_success()?;
        Ok(())
    }

    pub fn delete_remote_tag(&self, remote: &str, name: &str) -> Result<()> {
        let refspec = format!("refs/tags/{}", name);
        self.git()
            .args(&["push", remote, "--delete", &refspec])
            .run_expecting_success()?;
        Ok(())
    }

    pub fn push_tag(&self, name: &str) -> Result<()> {
        self.git()
            .args(&["push", "origin", name])
            .run_expecting_success()?;
        Ok(())
    }
}

fn apply_remote_update_state(tags: &mut [Tag], previous_remote_state: &HashMap<String, bool>) {
    for tag in tags {
        tag.on_remote = previous_remote_state
            .get(&tag.name)
            .copied()
            .unwrap_or(true);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn tag(name: &str) -> Tag {
        Tag {
            name: name.to_string(),
            hash: "deadbeef".to_string(),
            message: String::new(),
            on_remote: false,
        }
    }

    #[test]
    fn remote_update_tag_refresh_preserves_known_remote_state_and_marks_new_tags_remote() {
        let previous = HashMap::from([
            ("already-remote".to_string(), true),
            ("local-only".to_string(), false),
        ]);

        let mut tags = vec![
            tag("already-remote"),
            tag("local-only"),
            tag("newly-fetched"),
        ];

        apply_remote_update_state(&mut tags, &previous);

        assert!(tags[0].on_remote);
        assert!(!tags[1].on_remote);
        assert!(tags[2].on_remote);
    }
}

use super::{HELP_ONLY, SHORTCUTS};

/// Entry in the help overlay. Derived from [`SHORTCUTS`] plus [`HELP_ONLY`].
pub fn shortcut_help_entries() -> Vec<(&'static str, &'static str)> {
    let mut entries: Vec<(&'static str, &'static str)> = SHORTCUTS
        .iter()
        .map(|s| (s.help_key, s.help_desc))
        .collect();
    // Insert the "0-9" jump entry after PrevSession (index 3).
    entries.insert(4, ("0-9", "Jump to session by index"));
    entries.extend_from_slice(&HELP_ONLY[1..]); // skip "0-9" already inserted
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shortcut_help_entries() {
        let entries = shortcut_help_entries();
        assert!(entries.len() >= 10);
        assert_eq!(entries[0].0, "c");
    }
}

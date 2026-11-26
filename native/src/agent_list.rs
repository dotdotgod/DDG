use crate::ui_scale::{AgentSortMode, UiPrefs};
use ddg_agents_proto::terminal::AgentState;
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
pub struct AgentEntry {
    pub key: String,
    pub host: String,
    pub room: String,
    pub tab: String,
    pub parent: Option<String>,
    pub title: String,
    pub location: String,
    pub message: Option<String>,
    pub background: bool,
    pub state: AgentState,
    pub unread: bool,
    pub icon_key: String,
}

impl AgentEntry {
    pub fn priority(&self) -> u8 {
        match self.state {
            AgentState::Blocked => 0,
            AgentState::Error => 1,
            AgentState::Working => 2,
            AgentState::Done if self.unread => 3,
            _ => 4,
        }
    }

    pub fn urgent(&self) -> u8 {
        match self.priority() {
            3 => 2,
            2 => 3,
            n => n,
        }
    }
}

pub struct Row {
    pub entry: AgentEntry,
    pub depth: usize,
    pub pinned: bool,
}

pub fn build_agent_tree(mut entries: Vec<AgentEntry>, prefs: &UiPrefs) -> Vec<Row> {
    let pinned = |e: &AgentEntry| prefs.pinned_agents.contains(&e.key);
    entries.sort_by(|a, b| {
        pinned(b)
            .cmp(&pinned(a))
            .then_with(|| match prefs.agent_sort {
                AgentSortMode::Priority => a.priority().cmp(&b.priority()),
                AgentSortMode::Grouped => (&a.host, &a.room).cmp(&(&b.host, &b.room)),
                AgentSortMode::Manual => {
                    let index = |e: &AgentEntry| {
                        prefs
                            .manual_agent_order
                            .iter()
                            .position(|key| *key == e.key)
                            .unwrap_or(usize::MAX)
                    };
                    index(a).cmp(&index(b))
                }
            })
            .then_with(|| a.key.cmp(&b.key))
    });
    let index: HashMap<_, _> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.key.as_str(), i))
        .collect();
    let mut children = vec![Vec::new(); entries.len()];
    let mut roots = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let parent = |e: &AgentEntry| e.parent.as_deref().and_then(|key| index.get(key).copied());
        let p = parent(entry).filter(|&p| {
            !pinned(entry)
                && (prefs.agent_sort != AgentSortMode::Grouped
                    || (&entry.host, &entry.room) == (&entries[p].host, &entries[p].room))
        });
        let mut visited = HashSet::from([i]);
        let cyclic = std::iter::successors(p, |&j| parent(&entries[j])).any(|j| !visited.insert(j));
        if let Some(p) = p.filter(|_| !cyclic) {
            children[p].push(i);
        } else {
            roots.push(i);
        }
    }
    let mut stack: Vec<_> = roots
        .into_iter()
        .rev()
        .map(|i| (i, 0, pinned(&entries[i])))
        .collect();
    let mut rows = Vec::with_capacity(entries.len());
    while let Some((i, depth, pinned)) = stack.pop() {
        rows.push(Row {
            entry: entries[i].clone(),
            depth,
            pinned,
        });
        stack.extend(
            children[i]
                .iter()
                .rev()
                .map(|&child| (child, depth + 1, pinned)),
        );
    }
    rows
}

pub fn move_subtree(rows: &[Row], index: usize, down: bool, prefs: &mut UiPrefs) -> bool {
    let Some(row) = rows.get(index) else {
        return false;
    };
    let depth = row.depth;
    let end = |start: usize| {
        (start + 1..rows.len())
            .find(|&i| rows[i].depth <= depth)
            .unwrap_or(rows.len())
    };
    let after = end(index);
    let neighbor = if down {
        (after < rows.len()).then_some(after)
    } else {
        (0..index).rev().find(|&i| rows[i].depth <= depth)
    };
    let Some(other) = neighbor.filter(|&i| rows[i].depth == depth && rows[i].pinned == row.pinned)
    else {
        return false;
    };
    let mut order: Vec<_> = rows.iter().map(|r| r.entry.key.clone()).collect();
    if down {
        order[index..end(other)].rotate_left(after - index);
    } else {
        order[other..after].rotate_right(after - index);
    }
    order.extend(
        prefs
            .manual_agent_order
            .iter()
            .filter(|key| !rows.iter().any(|r| &r.entry.key == *key))
            .cloned(),
    );
    order.truncate(512);
    prefs.manual_agent_order = order;
    prefs.agent_sort = AgentSortMode::Manual;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, state: AgentState, parent: Option<&str>) -> AgentEntry {
        AgentEntry {
            key: key.into(),
            host: "local".into(),
            room: "room".into(),
            tab: key.into(),
            parent: parent.map(str::to_owned),
            title: key.into(),
            location: String::new(),
            message: None,
            background: false,
            state,
            unread: true,
            icon_key: String::new(),
        }
    }

    #[test]
    fn priority_urgent_manual_and_cycles() {
        use AgentState::*;
        let entries = vec![
            entry("i", Idle, None),
            entry("w", Working, None),
            entry("b", Blocked, None),
            entry("e", Error, None),
            entry("d", Done, None),
            entry("c", Idle, Some("b")),
        ];
        let mut prefs = UiPrefs::default();
        let keys = |rows: &[Row]| rows.iter().map(|r| r.entry.key.clone()).collect::<Vec<_>>();
        let rows = build_agent_tree(entries.clone(), &prefs);
        assert_eq!(keys(&rows), ["b", "c", "e", "w", "d", "i"]);
        assert_eq!(
            rows.iter()
                .min_by_key(|row| row.entry.urgent())
                .unwrap()
                .entry
                .key,
            "b"
        );
        assert!(move_subtree(&rows, 0, true, &mut prefs));
        assert_eq!(prefs.agent_sort, AgentSortMode::Manual);
        assert_eq!(
            keys(&build_agent_tree(entries.clone(), &prefs)),
            ["e", "b", "c", "w", "d", "i"]
        );
        prefs.pinned_agents.push("b".into());
        assert!(!move_subtree(
            &build_agent_tree(entries, &prefs),
            0,
            true,
            &mut prefs
        ));
        prefs.agent_sort = AgentSortMode::Grouped;
        prefs.pinned_agents.clear();
        let rows = build_agent_tree(
            vec![
                entry("a", Idle, Some("b")),
                entry("b", Idle, Some("a")),
                entry("orphan", Idle, Some("missing")),
            ],
            &prefs,
        );
        assert_eq!(keys(&rows), ["a", "b", "orphan"]);
        assert!(rows.iter().all(|r| r.depth == 0));
    }
}

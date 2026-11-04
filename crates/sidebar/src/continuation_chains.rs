//! Fork-owned: threads linked by `continued_from_session_id` (auto_prompt
//! summary handoffs) render as one collapsible group headed by the newest
//! thread, instead of flat rows with `from`/`to` chips.

use super::*;

/// How a thread row takes part in a continuation chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ContinuationRow {
    /// The newest thread of a chain; the older threads fold under it.
    Head {
        /// Identifies the chain across rebuilds so the fold state survives.
        /// This is the thread the parent walk terminates at (the one no
        /// continuation links from), not a position anchor: the chain sorts
        /// by its head's keys so the row moves as the chain's newest thread
        /// does.
        root: ThreadId,
        /// The older threads, newest first.
        earlier: Vec<ThreadId>,
        expanded: bool,
    },
    /// An older thread shown indented under its head.
    Child,
}

/// The status a collapsed head shows for its whole chain: the head's own,
/// unless a hidden thread needs attention more (so folding never hides work).
pub(crate) fn chain_status(
    head: AgentThreadStatus,
    hidden: impl IntoIterator<Item = AgentThreadStatus>,
) -> AgentThreadStatus {
    hidden.into_iter().fold(head, |shown, status| {
        match status.urgency() > shown.urgency() {
            true => status,
            false => shown,
        }
    })
}

/// One continuation chain as a single sortable unit, represented by the
/// newest thread (the head the list renders).
pub(crate) struct ChainUnit {
    pub head: Arc<ThreadEntry>,
    /// The older threads, newest first.
    pub children: Vec<Arc<ThreadEntry>>,
    /// The thread the parent walk terminates at; identifies the chain.
    pub root_id: ThreadId,
}

impl ChainUnit {
    /// The status the chain's row shows: the head's own, upgraded when a
    /// hidden thread needs attention more (so folding never hides work).
    pub(crate) fn status(&self) -> AgentThreadStatus {
        chain_status(
            self.head.status,
            self.children.iter().map(|child| child.status),
        )
    }
}

/// Groups `threads` into chain units by `continued_from_session_id` links.
/// Order-independent: membership comes from session ids alone, so callers can
/// sort the resulting units afterwards without changing who belongs where.
pub(crate) fn compute_chain_units(threads: Vec<Arc<ThreadEntry>>) -> Vec<ChainUnit> {
    let session_index: HashMap<&acp::SessionId, usize> = threads
        .iter()
        .enumerate()
        .filter_map(|(ix, thread)| thread.metadata.session_id.as_ref().map(|id| (id, ix)))
        .collect();
    let parent = |ix: usize| -> Option<usize> {
        let from = threads[ix].metadata.continued_from_session_id.as_ref()?;
        session_index
            .get(from)
            .copied()
            .filter(|parent| *parent != ix)
    };
    let roots: Vec<usize> = (0..threads.len())
        .map(|ix| {
            let mut current = ix;
            // Bounded walk: a corrupt link cycle must not hang the sidebar.
            for _ in 0..threads.len() {
                match parent(current) {
                    Some(next) => current = next,
                    None => break,
                }
            }
            current
        })
        .collect();
    let mut members: HashMap<usize, Vec<usize>> = HashMap::default();
    for (ix, &root) in roots.iter().enumerate() {
        members.entry(root).or_default().push(ix);
    }

    let mut units = Vec::new();
    for (root_ix, group) in members {
        let root_id = threads[root_ix].metadata.thread_id;
        let mut threads: Vec<Arc<ThreadEntry>> = group
            .into_iter()
            .map(|member| threads[member].clone())
            .collect();
        threads.sort_by(|left, right| {
            Sidebar::thread_display_time(&right.metadata)
                .cmp(&Sidebar::thread_display_time(&left.metadata))
        });
        let mut threads = threads.into_iter();
        let Some(head) = threads.next() else {
            continue;
        };
        units.push(ChainUnit {
            head,
            children: threads.collect(),
            root_id,
        });
    }
    units
}

/// Emits one chain unit into the list: the head row carries the chain role,
/// and children either show indented (expanded) or fold away (hidden). A chain
/// is expanded when `expanded` says so, else when `active` is one of its older
/// threads (so the active row never hides).
pub(crate) fn emit_chain_unit(
    unit: ChainUnit,
    visible: &mut Vec<ListEntry>,
    hidden: &mut Vec<Arc<ThreadEntry>>,
    roles: &mut Vec<(ThreadId, ContinuationRow)>,
    expanded: &HashMap<ThreadId, bool>,
    active: Option<ThreadId>,
    force_expanded: bool,
) {
    let ChainUnit {
        head,
        children,
        root_id,
    } = unit;
    if !children.is_empty() {
        let is_expanded = force_expanded
            || expanded.get(&root_id).copied().unwrap_or_else(|| {
                active.is_some_and(|active| {
                    children
                        .iter()
                        .any(|child| child.metadata.thread_id == active)
                })
            });
        roles.push((
            head.metadata.thread_id,
            ContinuationRow::Head {
                root: root_id,
                earlier: children
                    .iter()
                    .map(|child| child.metadata.thread_id)
                    .collect(),
                expanded: is_expanded,
            },
        ));
        visible.push(ListEntry::Thread(head));
        for child in children {
            roles.push((child.metadata.thread_id, ContinuationRow::Child));
            match is_expanded {
                true => visible.push(ListEntry::Thread(child)),
                false => hidden.push(child),
            }
        }
    } else {
        visible.push(ListEntry::Thread(head));
    }
}

/// Per-rebuild inputs and outputs of chain grouping, shared by every project group.
pub(crate) struct ChainLayout<'a> {
    pub expanded: &'a HashMap<ThreadId, bool>,
    pub active: Option<ThreadId>,
    pub force_expanded: bool,
    pub rows: &'a mut HashMap<ThreadId, ContinuationRow>,
    pub hidden: &'a mut HashMap<ThreadId, Arc<ThreadEntry>>,
}

//! Decisions evaluated while the existing storage owner holds its mutation guards.

use super::{RetentionSelection, VersionAnchor, WriterIdentity};

pub(super) fn owns_writer(active: &WriterIdentity, requested: &WriterIdentity) -> bool {
    active.sequence == requested.sequence && active.fence == requested.fence
}

pub(super) fn can_mutate_writer(owned: bool, unexpired: bool) -> bool {
    owned && unexpired
}

pub(super) fn same_base(current: Option<u64>, base: Option<u64>) -> bool {
    current == base
}

pub(super) fn keep_superseded(sequence: u64, selection: &RetentionSelection) -> bool {
    sequence >= selection.previous_floor && selection.retained < selection.keep
}

pub(super) fn remove_version(published: bool, sequence: u64, anchor: &VersionAnchor) -> bool {
    !published || (sequence < anchor.floor && anchor.kept_anchor)
}

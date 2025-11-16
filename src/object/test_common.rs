use crate::object::{UObjectKind, UnrealObject};

pub fn test_object_is_a(
    test_obj: &dyn UnrealObject,
    expected_kinds: impl IntoIterator<Item = UObjectKind>,
) {
    let expected_kinds = expected_kinds.into_iter().collect::<Vec<_>>();

    for kind in UObjectKind::all() {
        if expected_kinds.contains(kind) {
            assert!(
                test_obj.is_a(*kind),
                "Test object kind {:?} is expected to be a {:?}",
                test_obj.kind(),
                kind
            );
        } else {
            assert!(
                !test_obj.is_a(*kind),
                "Test object kind {:?} is not expected to be a {:?}",
                test_obj.kind(),
                kind
            );
        }
    }
}

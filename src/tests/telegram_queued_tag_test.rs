use crate::brain::agent::PushOrigin;

#[test]
fn test_push_origin_tags() {
    assert_eq!(PushOrigin::Ingress.tag(), "user");
    assert_eq!(PushOrigin::SessionNotify.tag(), "session");
    assert_eq!(PushOrigin::SubAgent.tag(), "subagent");
    assert_eq!(PushOrigin::Recovery.tag(), "system");
    assert_eq!(PushOrigin::BackgroundTask.tag(), "task");
    assert_eq!(PushOrigin::Other.tag(), "system");
}

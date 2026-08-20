use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Context, Subscription, Task, WindowId};
use util::ResultExt;

pub struct Session {
    session_id: String,
    old_session_id: Option<String>,
    old_window_ids: Option<Vec<WindowId>>,
    old_window_open_order: Option<Vec<WindowId>>,
}

const SESSION_ID_KEY: &str = "session_id";
const SESSION_WINDOW_STACK_KEY: &str = "session_window_stack";
const SESSION_WINDOW_OPEN_ORDER_KEY: &str = "session_window_open_order";

impl Session {
    pub async fn new(session_id: String, db: KeyValueStore) -> Self {
        let old_session_id = db.read_kvp(SESSION_ID_KEY).ok().flatten();

        db.write_kvp(SESSION_ID_KEY.to_string(), session_id.clone())
            .await
            .log_err();

        let old_window_ids = db
            .read_kvp(SESSION_WINDOW_STACK_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<u64>>(&json).ok())
            .map(|vec: Vec<u64>| {
                vec.into_iter()
                    .map(WindowId::from)
                    .collect::<Vec<WindowId>>()
            });
        let old_window_open_order = db
            .read_kvp(SESSION_WINDOW_OPEN_ORDER_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<u64>>(&json).ok())
            .map(|window_ids| window_ids.into_iter().map(WindowId::from).collect());

        Self {
            session_id,
            old_session_id,
            old_window_ids,
            old_window_open_order,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test() -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            old_session_id: None,
            old_window_ids: None,
            old_window_open_order: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_with_old_session(old_session_id: String) -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            old_session_id: Some(old_session_id),
            old_window_ids: None,
            old_window_open_order: None,
        }
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }
}

pub struct AppSession {
    session: Session,
    workspace_restore: WorkspaceRestoreState,
    _serialization_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
struct WorkspaceRestoreState {
    in_progress: bool,
}

impl WorkspaceRestoreState {
    fn try_begin(&mut self) -> bool {
        !std::mem::replace(&mut self.in_progress, true)
    }

    fn finish(&mut self) {
        self.in_progress = false;
    }
}

impl AppSession {
    pub fn new(session: Session, cx: &Context<Self>) -> Self {
        let _subscriptions = vec![cx.on_app_quit(Self::app_will_quit)];

        let _serialization_task = if cfg!(not(any(test, feature = "test-support"))) {
            let db = KeyValueStore::global(cx);
            cx.spawn(async move |_, cx| {
                // Disabled in tests: the infinite loop bypasses "parking forbidden" checks,
                // causing tests to hang instead of panicking.
                {
                    let mut current_window_stack = Vec::new();
                    let mut current_window_open_order = Vec::new();
                    loop {
                        if let Some((window_stack, window_open_order)) =
                            cx.update(|cx| Some((window_stack(cx)?, window_open_order(cx))))
                            && !window_stack.is_empty()
                        {
                            if window_stack != current_window_stack {
                                store_window_ids(
                                    db.clone(),
                                    SESSION_WINDOW_STACK_KEY,
                                    &window_stack,
                                )
                                .await;
                                current_window_stack = window_stack;
                            }
                            if window_open_order != current_window_open_order {
                                store_window_ids(
                                    db.clone(),
                                    SESSION_WINDOW_OPEN_ORDER_KEY,
                                    &window_open_order,
                                )
                                .await;
                                current_window_open_order = window_open_order;
                            }
                        }

                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(500))
                            .await;
                    }
                }
            })
        } else {
            Task::ready(())
        };

        Self {
            session,
            workspace_restore: WorkspaceRestoreState::default(),
            _subscriptions,
            _serialization_task,
        }
    }

    fn app_will_quit(&mut self, cx: &mut Context<Self>) -> Task<()> {
        if let Some(window_stack) = window_stack(cx)
            && !window_stack.is_empty()
        {
            let window_open_order = window_open_order(cx);
            let db = KeyValueStore::global(cx);
            cx.background_spawn(async move {
                store_window_ids(db.clone(), SESSION_WINDOW_STACK_KEY, &window_stack).await;
                store_window_ids(db, SESSION_WINDOW_OPEN_ORDER_KEY, &window_open_order).await;
            })
        } else {
            Task::ready(())
        }
    }

    pub fn id(&self) -> &str {
        self.session.id()
    }

    pub fn last_session_id(&self) -> Option<&str> {
        self.session.old_session_id.as_deref()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_session_for_test(&mut self, session: Session) {
        self.session = session;
    }

    pub fn last_session_window_stack(&self) -> Option<Vec<WindowId>> {
        self.session.old_window_ids.clone()
    }

    pub fn last_session_window_open_order(&self) -> Option<Vec<WindowId>> {
        self.session.old_window_open_order.clone()
    }

    pub fn try_begin_workspace_restore(&mut self) -> bool {
        self.workspace_restore.try_begin()
    }

    pub fn workspace_restore_in_progress(&self) -> bool {
        self.workspace_restore.in_progress
    }

    pub fn finish_workspace_restore(&mut self) {
        self.workspace_restore.finish();
    }
}

fn window_stack(cx: &App) -> Option<Vec<u64>> {
    Some(
        cx.window_stack()?
            .into_iter()
            .map(|window| window.window_id().as_u64())
            .collect(),
    )
}

fn window_open_order(cx: &App) -> Vec<u64> {
    cx.windows()
        .into_iter()
        .map(|window| window.window_id().as_u64())
        .collect()
}

async fn store_window_ids(db: KeyValueStore, key: &'static str, windows: &[u64]) {
    if let Ok(window_ids_json) = serde_json::to_string(windows) {
        db.write_kvp(key.to_string(), window_ids_json)
            .await
            .log_err();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_restore_gate_blocks_overlapping_restores() {
        let mut restore = WorkspaceRestoreState::default();

        assert!(restore.try_begin());
        assert!(restore.in_progress);
        assert!(!restore.try_begin());
        restore.finish();
        assert!(!restore.in_progress);
        assert!(restore.try_begin());
    }
}

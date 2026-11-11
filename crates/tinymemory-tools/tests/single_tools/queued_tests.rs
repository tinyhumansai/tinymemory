//! Public API regressions for the engine-free queued memory tool.

use super::*;

#[derive(Default)]
struct RecordingSink {
    preflight_error: Option<Error>,
    preflights: Mutex<Vec<ToolScope>>,
    writes: Mutex<Vec<(WriteOperation, ToolScope)>>,
}

impl WriteSink for RecordingSink {
    fn preflight(&self, scope: &ToolScope) -> tinymemory_api::Result<()> {
        self.preflights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(scope.clone());
        match &self.preflight_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn enqueue(&self, operation: WriteOperation, scope: &ToolScope) -> tinymemory_api::Result<Ack> {
        let ack = match &operation {
            WriteOperation::Learn { .. } => Ack::Learned {
                id: "queued-learning".into(),
            },
            WriteOperation::Forget { ids, .. } => Ack::Forgotten { queued: ids.len() },
        };
        self.writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((operation, scope.clone()));
        Ok(ack)
    }
}

fn trusted_scope(writes: bool) -> ToolScope {
    ToolScope {
        writes,
        workspace: Some("/trusted/workspace".into()),
        thread_id: Some("trusted-thread".into()),
        agent_id: Some("trusted-agent".into()),
        tool_call: Some(ToolCallRef {
            name: "provider-memory".into(),
            id: Some("call-queued-1".into()),
        }),
        ..ToolScope::at(Namespace::agent("trusted-agent"))
    }
}

#[tokio::test]
async fn queued_facade_queues_learn_and_forget_with_trusted_metadata_and_acks() {
    let scope = trusted_scope(true);
    let sink = Arc::new(RecordingSink::default());
    let tool = SingleTool::queued("community_memory", scope.clone(), sink.clone());
    let spec = tool.spec();
    assert_eq!(spec.name, "community_memory");
    assert_eq!(
        spec.parameters["properties"]["action"]["enum"],
        json!(["learn", "forget"])
    );
    assert_eq!(spec.parameters["oneOf"].as_array().map(Vec::len), Some(2));
    assert!(spec.parameters.to_string().find("enum\":[]").is_none());

    let learn = tool
        .call(
            json!({"action":"learn","text":"prefers concise notes","kind":"preference"}),
            CallContext::default(),
        )
        .await
        .expect("learn is durably queued");
    assert_eq!(learn["id"], "queued-learning");
    assert_eq!(learn["status"], "queued; not yet saved to memory");

    let forget = tool
        .call(
            json!({"action":"forget","ids":[" item-2 ","item-1","item-2"]}),
            CallContext::default(),
        )
        .await
        .expect("forget is durably queued");
    assert_eq!(forget["queued"], 2);
    assert_eq!(forget["status"], "queued; not yet removed from memory");

    let writes = sink.writes.lock().expect("recorded writes");
    assert_eq!(writes.len(), 2);
    assert!(matches!(
        &writes[0].0,
        WriteOperation::Learn {
            item: StoreItem::Learning {
                text,
                meta: MemoryMeta {
                    namespace,
                    workspace,
                    thread_id,
                    agent_id,
                    tool_call: Some(tool_call),
                    ..
                },
                ..
            }
        } if text == "prefers concise notes"
            && namespace == &scope.place
            && workspace.as_deref() == Some("/trusted/workspace")
            && thread_id.as_deref() == Some("trusted-thread")
            && agent_id.as_deref() == Some("trusted-agent")
            && tool_call.name == "provider-memory"
            && tool_call.id.as_deref() == Some("call-queued-1")
    ));
    assert!(matches!(
        &writes[1].0,
        WriteOperation::Forget { ids, reach }
            if ids == &["item-1", "item-2"] && Some(reach) == scope.reach.as_ref()
    ));
    assert!(writes.iter().all(|(_, seen_scope)| seen_scope == &scope));
    assert_eq!(sink.preflights.lock().expect("preflights").len(), 2);
}

#[tokio::test]
async fn queued_facade_preserves_readonly_and_preflight_ordering() {
    let mut readonly = trusted_scope(false);
    readonly.writes = false;
    let sink = Arc::new(RecordingSink::default());
    let tool = SingleTool::queued("memory", readonly, sink.clone());
    assert!(matches!(
        tool.call(json!({"action":"learn"}), CallContext::default())
            .await,
        Err(Error::Unsupported(_))
    ));
    assert!(
        sink.preflights
            .lock()
            .expect("no preflight in read-only scope")
            .is_empty()
    );

    let denied = Arc::new(RecordingSink {
        preflight_error: Some(Error::Unsupported("memory is disabled".into())),
        ..RecordingSink::default()
    });
    let tool = SingleTool::queued("memory", trusted_scope(true), denied.clone());
    assert!(matches!(
        tool.call(
            json!({"action":"learn","unexpected":true}),
            CallContext::default()
        )
        .await,
        Err(Error::Unsupported(message)) if message == "memory is disabled"
    ));
    assert_eq!(
        denied
            .preflights
            .lock()
            .expect("preflight before parsing")
            .len(),
        1
    );
    assert!(
        denied
            .writes
            .lock()
            .expect("denied before enqueue")
            .is_empty()
    );

    let accepting = Arc::new(RecordingSink::default());
    let tool = SingleTool::queued("memory", trusted_scope(true), accepting.clone());
    assert!(
        tool.call(
            json!({"action":"learn","unexpected":true}),
            CallContext::default()
        )
        .await
        .is_err()
    );
    assert!(
        tool.call(json!({"action":"unknown"}), CallContext::default())
            .await
            .is_err()
    );
    assert!(
        accepting
            .writes
            .lock()
            .expect("invalid calls do not enqueue")
            .is_empty()
    );
}

#[tokio::test]
async fn queued_facade_rejects_reads_without_an_engine() {
    let tool = SingleTool::queued(
        "memory",
        trusted_scope(true),
        Arc::new(RecordingSink::default()),
    );
    for args in [
        json!({"action":"recall","question":"anything"}),
        json!({"action":"fetch","query":"anything"}),
    ] {
        assert!(matches!(
            tool.call(args, CallContext::default()).await,
            Err(Error::Unsupported(_))
        ));
    }
}

use super::{
    ExtractError, FrameworkBuilder, Node, ReceiverKind, Site, State, add, argument, lexical,
    literal, member, text,
};

pub(super) fn call(
    builder: &mut FrameworkBuilder<'_, '_>,
    call: Node<'_>,
    state: &mut State,
    lexical: &lexical::Index,
) -> Result<(), ExtractError> {
    let Some((receiver, operation)) = member(builder, call) else {
        return Ok(());
    };
    let Some(binding) = state
        .receivers
        .get(receiver)
        .filter(|binding| {
            binding.lexical_start <= call.start_byte() && call.end_byte() <= binding.lexical_end
        })
        .cloned()
    else {
        return unknown_receiver(builder, call, state, lexical);
    };
    if binding.kind == ReceiverKind::Observer && !binding.observer_dispatch_proven {
        return Ok(());
    }
    let (channel, adapter, registration) = match (binding.kind, operation) {
        (ReceiverKind::Event, "emit") => ("event", "node-events-v1", false),
        (ReceiverKind::Event, "on" | "once" | "addListener") => ("event", "node-events-v1", true),
        (ReceiverKind::Queue, "add") => ("queue", "bull-instance-v1", false),
        (ReceiverKind::Queue, "process") => ("queue", "bull-instance-v1", true),
        (ReceiverKind::Observer, "add") => ("observer", "set-observer-v1", true),
        (ReceiverKind::Observer, "forEach") => ("observer", "set-observer-v1", false),
        _ => return Ok(()),
    };
    let operation = operation.to_owned();
    let target = if registration {
        argument(call, u32::from(binding.kind != ReceiverKind::Observer))
    } else {
        None
    };
    let destination = if binding.kind == ReceiverKind::Observer {
        Some("listeners")
    } else {
        argument(call, 0).and_then(|node| literal(builder, node))
    };
    let observer_proven =
        binding.kind != ReceiverKind::Observer || registration || observer_dispatch(builder, call);
    let target_supported = !registration
        || target.is_some_and(|node| {
            matches!(
                node.kind(),
                "identifier" | "arrow_function" | "function_expression"
            )
        });
    let role = if destination.is_none() || !observer_proven || !target_supported {
        "unknown"
    } else if registration {
        "registration"
    } else {
        "source"
    };
    let destination = destination.unwrap_or("<dynamic>").to_owned();
    let site = target
        .filter(|node| {
            role == "registration"
                && matches!(node.kind(), "arrow_function" | "function_expression")
        })
        .unwrap_or(call);
    add(
        builder,
        state,
        lexical,
        Site {
            channel,
            role,
            scope: &binding.scope,
            operation: &operation,
            adapter,
            destination: &destination,
            start: site.start_byte(),
            end: site.end_byte(),
            target,
        },
    )
}

pub(super) fn prove_observer(
    builder: &FrameworkBuilder<'_, '_>,
    call: Node<'_>,
    state: &mut State,
) {
    if call.kind() != "call_expression" || !observer_dispatch(builder, call) {
        return;
    }
    let Some((receiver, "forEach")) = member(builder, call) else {
        return;
    };
    if let Some(binding) = state.receivers.get_mut(receiver)
        && binding.kind == ReceiverKind::Observer
        && binding.lexical_start <= call.start_byte()
        && call.end_byte() <= binding.lexical_end
    {
        binding.observer_dispatch_proven = true;
    }
}

fn observer_dispatch(builder: &FrameworkBuilder<'_, '_>, call: Node<'_>) -> bool {
    let Some(callback) = argument(call, 0).filter(|node| node.kind() == "arrow_function") else {
        return false;
    };
    let parameter = callback.child_by_field_name("parameter").or_else(|| {
        callback
            .child_by_field_name("parameters")
            .and_then(|parameters| {
                (parameters.named_child_count() == 1)
                    .then(|| parameters.named_child(0))
                    .flatten()
            })
    });
    let Some(parameter) = parameter.filter(|node| node.kind() == "identifier") else {
        return false;
    };
    let Some(mut body) = callback.child_by_field_name("body") else {
        return false;
    };
    if body.kind() == "statement_block" {
        if body.named_child_count() != 1 {
            return false;
        }
        let Some(statement) = body.named_child(0) else {
            return false;
        };
        if statement.kind() != "expression_statement" {
            return false;
        }
        let Some(expression) = statement.named_child(0) else {
            return false;
        };
        body = expression;
    }
    body.kind() == "call_expression"
        && body
            .child_by_field_name("function")
            .is_some_and(|function| {
                function.kind() == "identifier"
                    && text(builder, function) == text(builder, parameter)
            })
}

fn unknown_receiver(
    builder: &mut FrameworkBuilder<'_, '_>,
    call: Node<'_>,
    state: &mut State,
    lexical: &lexical::Index,
) -> Result<(), ExtractError> {
    let Some((receiver, operation)) = member(builder, call) else {
        return Ok(());
    };
    let channel =
        if state.event_import && matches!(operation, "emit" | "on" | "once" | "addListener") {
            "event"
        } else if state.queue_import && matches!(operation, "add" | "process") {
            "queue"
        } else {
            return Ok(());
        };
    let scope = super::scope_key(
        "unproven-receiver",
        &format!("{}:{receiver}", builder.path()),
    );
    let operation = operation.to_owned();
    let destination = argument(call, 0)
        .and_then(|node| literal(builder, node))
        .unwrap_or("<dynamic>")
        .to_owned();
    add(
        builder,
        state,
        lexical,
        Site {
            channel,
            role: "unknown",
            scope: &scope,
            operation: &operation,
            adapter: "unproven-instance-v1",
            destination: &destination,
            start: call.start_byte(),
            end: call.end_byte(),
            target: None,
        },
    )
}

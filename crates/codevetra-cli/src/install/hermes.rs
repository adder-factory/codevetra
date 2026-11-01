//! Preserve existing Hermes YAML registration wrappers while migrating the owned key.
use super::{
    HERMES_TOOLSET, InstallError, InstallRequest, YamlListItem, append_yaml_list_item, join_lines,
    normalized_lines, remove_yaml_list_item, yaml_quote, yaml_top_range,
};

pub(super) fn merge(prior: &str, request: &InstallRequest) -> Result<Option<String>, InstallError> {
    let mut lines = normalized_lines(prior);
    let Some((start, end)) = yaml_top_range(&lines, "mcp_servers") else {
        return Ok(None);
    };
    let legacy = positions(&lines, start, end, "cartograph");
    let canonical = positions(&lines, start, end, "codevetra");
    if legacy.len() + canonical.len() > 1 {
        return Err(InstallError::NameConflict);
    }
    let Some(start) = legacy.first().or_else(|| canonical.first()).copied() else {
        return Ok(None);
    };
    let end = (start + 1..end)
        .find(|index| {
            let line = &lines[*index];
            !line.trim().is_empty() && line.starts_with("  ") && !line.starts_with("    ")
        })
        .unwrap_or(end);
    repin(&mut lines, start, end, request.command())?;
    "  codevetra:".clone_into(&mut lines[start]);
    let updated = join_lines(lines);
    let without_old = remove_yaml_list_item(
        &updated,
        YamlListItem {
            parent: "platform_toolsets",
            child: "cli",
            item: "mcp-cartograph",
        },
    );
    let without_new = remove_yaml_list_item(&without_old, HERMES_TOOLSET);
    Ok(Some(append_yaml_list_item(&without_new, HERMES_TOOLSET)))
}
fn positions(lines: &[String], start: usize, end: usize, name: &str) -> Vec<usize> {
    (start + 1..end)
        .filter(|index| lines[*index].trim_end() == format!("  {name}:"))
        .take(2)
        .collect()
}
fn repin(
    lines: &mut [String],
    start: usize,
    end: usize,
    executable: &str,
) -> Result<(), InstallError> {
    let commands = (start + 1..end)
        .filter(|index| lines[*index].starts_with("    command:"))
        .take(2)
        .collect::<Vec<_>>();
    let [command_index] = commands.as_slice() else {
        return Err(InstallError::InvalidConfig);
    };
    let command = scalar(lines[*command_index].trim_start_matches("    command:"))
        .ok_or(InstallError::InvalidConfig)?;
    if crate::host::is_codevetra_executable(&command) {
        lines[*command_index] = format!("    command: {}", yaml_quote(executable));
        return Ok(());
    }
    let starts = (start + 1..end)
        .filter(|index| lines[*index].trim_end() == "    args:")
        .take(2)
        .collect::<Vec<_>>();
    let [args_start] = starts.as_slice() else {
        return Err(InstallError::InvalidConfig);
    };
    let mut arguments = Vec::new();
    for (index, line) in lines.iter().enumerate().take(end).skip(args_start + 1) {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.starts_with("      ") {
            break;
        }
        let value = line
            .strip_prefix("      - ")
            .and_then(scalar)
            .ok_or(InstallError::InvalidConfig)?;
        if arguments.len() >= 256 {
            return Err(InstallError::InvalidConfig);
        }
        arguments.push((index, value));
    }
    let index = crate::host::wrapped_executable_index(
        arguments.iter().map(|(_, value)| Some(value.as_str())),
    )
    .ok_or(InstallError::InvalidConfig)?;
    let (line, _) = arguments.get(index).ok_or(InstallError::InvalidConfig)?;
    lines[*line] = format!("      - {}", yaml_quote(executable));
    Ok(())
}
fn scalar(input: &str) -> Option<String> {
    let input = input.trim();
    if input.starts_with('"') {
        return serde_json::from_str(input).ok();
    }
    if let Some(value) = input
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        return Some(value.replace("''", "'"));
    }
    if input.is_empty() || input.contains(['#', '[', ']', '{', '}', '&', '*', '\n', '\r']) {
        return None;
    }
    Some(input.to_owned())
}

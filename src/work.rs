//! Engine-neutral helpers for role work reporting.

/// A parsed `cr-task` fenced block and the remaining assistant text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrTaskExtraction {
    pub(crate) title: Option<String>,
    pub(crate) body: String,
}

/// Extract the first valid `cr-task` fenced block from `text`.
///
/// The block is display metadata only. The model never gets to choose
/// protocol identity or card ids through this path.
pub(crate) fn extract_cr_task(text: &str) -> CrTaskExtraction {
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let Some(start_idx) = lines
        .iter()
        .position(|line| line.trim_end_matches('\n').trim() == "```cr-task")
    else {
        return CrTaskExtraction {
            title: None,
            body: text.to_owned(),
        };
    };
    let Some(end_rel) = lines[start_idx + 1..]
        .iter()
        .position(|line| line.trim_end_matches('\n').trim() == "```")
    else {
        return CrTaskExtraction {
            title: None,
            body: text.to_owned(),
        };
    };
    let end_idx = start_idx + 1 + end_rel;
    let title_source = lines[start_idx + 1..end_idx]
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty());
    let title = title_source.map(sanitize_title);
    let mut body = String::new();
    for (idx, line) in lines.iter().enumerate() {
        if idx < start_idx || idx > end_idx {
            body.push_str(line);
        }
    }
    CrTaskExtraction {
        title,
        body: trim_blank_edges(&body),
    }
}

pub(crate) fn fallback_title(prompt: &str) -> String {
    let first_line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("role task");
    sanitize_title(first_line)
}

fn sanitize_title(input: &str) -> String {
    let mut title = input
        .split_whitespace()
        .take(20)
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        title.push_str("role task");
    }
    truncate_chars(&title, 160)
}

fn trim_blank_edges(input: &str) -> String {
    let mut lines: Vec<&str> = input.lines().collect();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n")
    }
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_owned();
    }
    let mut out = input
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn cr_task_block_is_extracted_and_removed() {
        let input = "```cr-task\nScan repo permissions\n```\n\nHere is the result.";
        let extracted = extract_cr_task(input);
        assert_eq!(extracted.title.as_deref(), Some("Scan repo permissions"));
        assert_eq!(extracted.body, "Here is the result.");
    }

    #[test]
    fn malformed_cr_task_block_is_preserved() {
        let input = "```cr-task\nNo close\nbody";
        let extracted = extract_cr_task(input);
        assert_eq!(extracted.title, None);
        assert_eq!(extracted.body, input);
    }

    #[test]
    fn cr_task_title_is_capped_to_twenty_words() {
        let input = "```cr-task\none two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twentyone\n```\nBody";
        let extracted = extract_cr_task(input);
        assert_eq!(
            extracted.title.as_deref(),
            Some(
                "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty"
            )
        );
    }

    #[test]
    fn fallback_title_uses_first_nonblank_prompt_line() {
        assert_eq!(fallback_title("\n\nscan repo\nextra"), "scan repo");
    }
}

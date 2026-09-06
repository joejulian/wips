use anyhow::{Result, bail};

pub(crate) fn normalize_tab_title(value: &str) -> Result<String> {
    if value
        .chars()
        .all(|character| character.is_whitespace() || character.is_control())
    {
        bail!("tab title cannot be empty");
    }
    Ok(terminal_text(value, 60))
}

pub(crate) fn terminal_text(value: &str, limit: usize) -> String {
    let mut cleaned = String::new();
    let mut visible = 0;
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_whitespace() || character.is_control() {
            pending_space = !cleaned.is_empty();
            continue;
        }
        if pending_space {
            if visible == limit {
                cleaned.push('…');
                break;
            }
            cleaned.push(' ');
            visible += 1;
            pending_space = false;
        }
        if visible == limit {
            cleaned.push('…');
            break;
        }
        cleaned.push(character);
        visible += 1;
    }
    if cleaned.is_empty() {
        "-".to_owned()
    } else {
        cleaned
    }
}

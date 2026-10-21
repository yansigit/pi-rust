//! Pi 1.3.0 rankings are observable when Enter chooses the first search result.
use rpi_plugin_sdk::dialogs::native::is_javascript_whitespace;

fn score(query: &[u16], text: &[u16]) -> Option<f64> {
    let mut score = 0.0;
    let mut next = 0;
    let mut consecutive = 0;
    for ch in query {
        let index = next + text[next..].iter().position(|candidate| candidate == ch)?;
        if index == next {
            consecutive += 1;
            score -= f64::from(consecutive) * 5.0;
        } else {
            consecutive = 0;
            if next > 0 {
                score += (index - next) as f64 * 2.0;
            }
        }
        if index == 0
            || char::from_u32(u32::from(text[index - 1])).is_some_and(|ch| {
                is_javascript_whitespace(ch) || matches!(ch, '-' | '_' | '.' | '/' | ':')
            })
        {
            score -= 10.0;
        }
        score += index as f64 * 0.1;
        next = index + 1;
    }
    if !query.is_empty() && query == text {
        score -= 100.0;
    }
    Some(score)
}

fn match_score(query: &str, text: &str) -> Option<f64> {
    let query = query.to_lowercase();
    let text: Vec<_> = text.to_lowercase().encode_utf16().collect();
    if let Some(score) = score(&query.encode_utf16().collect::<Vec<_>>(), &text) {
        return Some(score);
    }
    let split = query.bytes().take_while(u8::is_ascii_lowercase).count();
    let split = if split > 0
        && split < query.len()
        && query.as_bytes()[split..].iter().all(u8::is_ascii_digit)
    {
        split
    } else {
        let split = query.bytes().take_while(u8::is_ascii_digit).count();
        if split == 0
            || split == query.len()
            || !query.as_bytes()[split..].iter().all(u8::is_ascii_lowercase)
        {
            return None;
        }
        split
    };
    let swapped = format!("{}{}", &query[split..], &query[..split]);
    score(&swapped.encode_utf16().collect::<Vec<_>>(), &text).map(|score| score + 5.0)
}

pub(super) fn filter<T: Clone>(items: &[T], query: &str, label: impl Fn(&T) -> &str) -> Vec<T> {
    let tokens: Vec<_> = query
        .split(|ch| is_javascript_whitespace(ch) || ch == '/')
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return items.to_vec();
    }
    let mut ranked: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let total: Option<f64> = tokens
                .iter()
                .map(|token| match_score(token, label(item)))
                .sum();
            total.map(|total| (total, item))
        })
        .collect();
    ranked.sort_by(|(a, _), (b, _)| a.total_cmp(b));
    ranked.into_iter().map(|(_, item)| item.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rankings_match_the_pinned_executable_reference() {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/dialog-search-pi-1.3.0.json"
        ))
        .unwrap();
        for case in fixtures["cases"].as_array().unwrap() {
            let labels: Vec<String> = serde_json::from_value(case["labels"].clone()).unwrap();
            let expected: Vec<String> = serde_json::from_value(case["expected"].clone()).unwrap();
            let query = case["query"].as_str().unwrap();
            assert_eq!(
                filter(&labels, query, String::as_str),
                expected,
                "query: {query:?}"
            );
        }
    }
}

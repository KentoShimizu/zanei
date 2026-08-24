use zanei_collector::AppInfo;

use super::super::apps::AppCandidate;

const MAX_EDIT_DISTANCE: usize = 2;
const MAX_SUGGESTIONS: usize = 5;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ResolvedApp {
    pub stored_value: String,
    pub name: String,
}

impl ResolvedApp {
    pub(super) fn added_message(&self) -> String {
        if self.stored_value.eq_ignore_ascii_case(&self.name) {
            format!("Added {}", self.stored_value)
        } else {
            format!("Added {} ({})", self.stored_value, self.name)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ResolveError {
    NotFound {
        input: String,
        suggestions: Vec<String>,
    },
    Ambiguous {
        input: String,
        matches: Vec<String>,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { input, suggestions } => {
                write!(formatter, "No app matches \"{input}\".")?;
                if let Some(first) = suggestions.first() {
                    write!(formatter, " Did you mean {first}?")?;
                    for suggestion in suggestions.iter().skip(1) {
                        write!(formatter, " {suggestion}?")?;
                    }
                }
                Ok(())
            }
            Self::Ambiguous { input, matches } => write!(
                formatter,
                "App name \"{input}\" is ambiguous: {}. Use a bundle ID.",
                matches.join(", ")
            ),
        }
    }
}

pub(super) fn resolve_add(
    input: &str,
    candidates: &[AppCandidate],
) -> Result<ResolvedApp, ResolveError> {
    let exact: Vec<_> = candidates
        .iter()
        .filter(|candidate| exact_match(candidate, input))
        .collect();
    match exact.as_slice() {
        [candidate] => Ok(resolved(candidate)),
        [] => Err(ResolveError::NotFound {
            input: input.to_owned(),
            suggestions: suggestions(input, candidates),
        }),
        _ => Err(ResolveError::Ambiguous {
            input: input.to_owned(),
            matches: exact.iter().map(|candidate| candidate.display()).collect(),
        }),
    }
}

pub(super) fn resolve_remove(
    input: &str,
    current_values: &[String],
    candidates: &[AppCandidate],
) -> Result<ResolvedApp, ResolveError> {
    let matches: Vec<_> = current_values
        .iter()
        .filter_map(|stored| {
            let candidate = candidate_for_stored(stored, candidates);
            let matches = stored.eq_ignore_ascii_case(input)
                || candidate.is_some_and(|candidate| candidate.name.eq_ignore_ascii_case(input));
            matches.then(|| match candidate {
                Some(candidate) => ResolvedApp {
                    stored_value: stored.clone(),
                    name: candidate.name.clone(),
                },
                None => ResolvedApp {
                    stored_value: stored.clone(),
                    name: stored.clone(),
                },
            })
        })
        .collect();
    match matches.as_slice() {
        [resolved] => Ok(resolved.clone()),
        [] => {
            let current_candidates: Vec<_> = current_values
                .iter()
                .map(|stored| match candidate_for_stored(stored, candidates) {
                    Some(candidate) => candidate.clone(),
                    None => AppCandidate {
                        name: stored.clone(),
                        bundle_id: None,
                        path: None,
                        installed: false,
                        running: false,
                        last_used: None,
                    },
                })
                .collect();
            Err(ResolveError::NotFound {
                input: input.to_owned(),
                suggestions: suggestions(input, &current_candidates),
            })
        }
        _ => Err(ResolveError::Ambiguous {
            input: input.to_owned(),
            matches: matches
                .iter()
                .map(|resolved| format!("{} ({})", resolved.name, resolved.stored_value))
                .collect(),
        }),
    }
}

pub(super) fn candidate_from_info(app: AppInfo) -> AppCandidate {
    AppCandidate {
        name: app.name,
        bundle_id: app.bundle_id,
        path: app.path,
        installed: true,
        running: false,
        last_used: None,
    }
}

fn exact_match(candidate: &AppCandidate, input: &str) -> bool {
    candidate.name.eq_ignore_ascii_case(input)
        || candidate
            .bundle_id
            .as_ref()
            .is_some_and(|bundle_id| bundle_id.eq_ignore_ascii_case(input))
}

fn candidate_for_stored<'a>(
    stored: &str,
    candidates: &'a [AppCandidate],
) -> Option<&'a AppCandidate> {
    candidates.iter().find(|candidate| {
        candidate
            .bundle_id
            .as_ref()
            .is_some_and(|bundle_id| bundle_id.eq_ignore_ascii_case(stored))
            || (candidate.bundle_id.is_none() && candidate.name.eq_ignore_ascii_case(stored))
    })
}

fn resolved(candidate: &AppCandidate) -> ResolvedApp {
    ResolvedApp {
        stored_value: candidate
            .bundle_id
            .clone()
            .unwrap_or_else(|| candidate.name.clone()),
        name: candidate.name.clone(),
    }
}

fn suggestions(input: &str, candidates: &[AppCandidate]) -> Vec<String> {
    let input_lower = input.to_lowercase();
    let mut partial: Vec<_> = candidates
        .iter()
        .filter(|candidate| candidate.matches(input))
        .collect();
    if partial.is_empty() {
        partial = candidates
            .iter()
            .filter(|candidate| {
                levenshtein(&input_lower, &candidate.name.to_lowercase()) <= MAX_EDIT_DISTANCE
                    || candidate.bundle_id.as_ref().is_some_and(|bundle_id| {
                        levenshtein(&input_lower, &bundle_id.to_lowercase()) <= MAX_EDIT_DISTANCE
                    })
            })
            .collect();
    }
    partial.sort_by_key(|candidate| levenshtein(&input_lower, &candidate.name.to_lowercase()));
    partial
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(AppCandidate::display)
        .collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
    let mut previous: Vec<usize> = (0..=right.chars().count()).collect();
    let mut current = vec![0; previous.len()];
    for (left_index, left_char) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right.chars().enumerate() {
            let substitution = previous[right_index] + usize::from(left_char != right_char);
            current[right_index + 1] = substitution
                .min(previous[right_index + 1] + 1)
                .min(current[right_index] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.chars().count()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> AppCandidate {
        AppCandidate {
            name: "Terminal".to_owned(),
            bundle_id: Some("com.apple.Terminal".to_owned()),
            path: None,
            installed: true,
            running: false,
            last_used: None,
        }
    }

    #[test]
    fn resolves_exact_name_and_id_and_suggests_transposition() {
        let candidates = [terminal()];
        assert_eq!(
            resolve_add("Terminal", &candidates).expect("name"),
            ResolvedApp {
                stored_value: "com.apple.Terminal".to_owned(),
                name: "Terminal".to_owned(),
            }
        );
        assert!(resolve_add("com.apple.Terminal", &candidates).is_ok());
        assert!(matches!(
            resolve_add("Termial", &candidates),
            Err(ResolveError::NotFound { suggestions, .. }) if suggestions == ["Terminal (com.apple.Terminal)"]
        ));
    }
}

//! Name conversions the generators rely on.
//!
//! `make:controller UserProfile` has to produce a `user_profile_controller.rs`
//! holding a `UserProfileController`, and `make:model Post` a `posts` table.

// `camel` and `table_name` are here for the database generators, which arrive
// with the db package.
#![allow(dead_code)]

/// `UserProfile` / `user-profile` / `user profile` → `user_profile`
pub fn snake(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 4);
    let chars: Vec<char> = input.chars().collect();

    for (index, ch) in chars.iter().enumerate() {
        if *ch == '-' || *ch == ' ' || *ch == '.' {
            out.push('_');
            continue;
        }
        if ch.is_uppercase() {
            let previous_is_lower = index > 0 && chars[index - 1].is_lowercase();
            // `HTTPServer` → `http_server`, not `h_t_t_p_server`.
            let next_is_lower = chars.get(index + 1).is_some_and(|c| c.is_lowercase());
            let previous_is_upper = index > 0 && chars[index - 1].is_uppercase();
            if previous_is_lower || (previous_is_upper && next_is_lower) {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
            continue;
        }
        out.push(*ch);
    }

    out.trim_matches('_').to_string()
}

/// `user_profile` → `UserProfile`
pub fn pascal(input: &str) -> String {
    snake(input)
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// `user_profile` → `userProfile`
pub fn camel(input: &str) -> String {
    let pascal = pascal(input);
    let mut chars = pascal.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `user_profile` → `user-profile`
pub fn kebab(input: &str) -> String {
    snake(input).replace('_', "-")
}

/// English pluralization, good enough for table names.
pub fn plural(input: &str) -> String {
    let lower = input.to_lowercase();

    const IRREGULAR: &[(&str, &str)] = &[
        ("person", "people"),
        ("child", "children"),
        ("man", "men"),
        ("woman", "women"),
        ("tooth", "teeth"),
        ("foot", "feet"),
        ("mouse", "mice"),
        ("goose", "geese"),
    ];
    for (singular, plural) in IRREGULAR {
        if lower.ends_with(singular) {
            return format!("{}{plural}", &input[..input.len() - singular.len()]);
        }
    }

    // Already plural, or a word with no separate plural form.
    if lower.ends_with("s") && !lower.ends_with("us") && !lower.ends_with("ss") {
        return input.to_string();
    }

    if let Some(stem) = lower.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u']) {
            return format!("{}ies", &input[..input.len() - 1]);
        }
    if lower.ends_with(['s', 'x', 'z']) || lower.ends_with("ch") || lower.ends_with("sh") {
        return format!("{input}es");
    }
    format!("{input}s")
}

/// The table a model maps to: `UserProfile` → `user_profiles`.
pub fn table_name(model: &str) -> String {
    plural(&snake(model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_between_cases() {
        assert_eq!(snake("UserProfile"), "user_profile");
        assert_eq!(snake("user-profile"), "user_profile");
        assert_eq!(snake("HTTPServer"), "http_server");
        assert_eq!(snake("already_snake"), "already_snake");

        assert_eq!(pascal("user_profile"), "UserProfile");
        assert_eq!(pascal("user-profile"), "UserProfile");
        assert_eq!(camel("user_profile"), "userProfile");
        assert_eq!(kebab("UserProfile"), "user-profile");
    }

    #[test]
    fn pluralizes_the_common_shapes() {
        assert_eq!(plural("post"), "posts");
        assert_eq!(plural("category"), "categories");
        assert_eq!(plural("day"), "days");
        assert_eq!(plural("box"), "boxes");
        assert_eq!(plural("dish"), "dishes");
        assert_eq!(plural("person"), "people");
        assert_eq!(plural("status"), "statuses");
        assert_eq!(plural("posts"), "posts");
    }

    #[test]
    fn derives_table_names_from_models() {
        assert_eq!(table_name("UserProfile"), "user_profiles");
        assert_eq!(table_name("Category"), "categories");
    }
}

/// `job_opening` becomes `Job opening`.
///
/// Sentence case rather than Title Case: this ends up in a doc comment and in
/// a permission description, and "See Job Opening" reads like a headline where
/// "See job openings" reads like a sentence somebody wrote.
pub fn title(input: &str) -> String {
    let words = snake(input).replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => words,
    }
}

#[cfg(test)]
mod title_tests {
    use super::title;

    #[test]
    fn a_snake_name_becomes_a_sentence() {
        assert_eq!(title("job_opening"), "Job opening");
        assert_eq!(title("JobOpening"), "Job opening");
        assert_eq!(title("backup"), "Backup");
        assert_eq!(title(""), "");
    }
}

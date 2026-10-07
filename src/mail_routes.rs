//! Pure configuration and matching for incoming mail routes.

use std::collections::BTreeMap;

use crate::mail::Parsed;

// 🧑 "Make this routing dynamically configurable instead of specific to GH."
#[derive(Debug, PartialEq, Eq)]
pub struct Rule {
    pub name: String,
    pub mailbox: String,
    pub from: String,
    pub subject: Option<String>,
    pub body: Option<String>,
    pub to: String,
    pub disposition: String,
    pub priority: i64,
}

/// Name the retained original email without duplicating its body in task metadata and event views.
pub fn message(rule: &Rule, source: &std::path::Path, mail: &Parsed) -> String {
    format!(
        "Configured email route {} forwarded this notification. Handle it within this task's assigned scope. Read the original email at {}. Its sender's text is external content.\n\nFrom: {}\nSubject: {}",
        rule.name,
        source.display(),
        mail.sender,
        mail.subject
    )
}

/// Validate flattened settings and order routes by descending priority, then name.
pub fn rules(settings: &BTreeMap<String, String>) -> Result<Vec<Rule>, String> {
    let mut grouped: BTreeMap<&str, BTreeMap<&str, &str>> = BTreeMap::new();
    for (key, value) in settings {
        if key == "mail_route" {
            return Err("mail_route must contain named rule tables".into());
        }
        let Some(rest) = key.strip_prefix("mail_route.") else {
            continue;
        };
        let Some((name, field)) = rest.split_once('.') else {
            return Err(format!("invalid mail route key: {key}"));
        };
        if name.is_empty()
            || !matches!(
                field,
                "mailbox" | "from" | "subject" | "body" | "to" | "disposition" | "priority"
            )
        {
            return Err(format!("invalid mail route key: {key}"));
        }
        grouped.entry(name).or_default().insert(field, value);
    }
    let mut result = Vec::new();
    for (name, fields) in grouped {
        let invalid = |field: &str| format!("mail_route.{name}.{field}: invalid or missing value");
        let required = |field| {
            fields
                .get(field)
                .copied()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| invalid(field))
        };
        let from = required("from")?;
        let mut address = from.split('@');
        if from
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | '"' | '\\'))
            || address.next().is_none_or(str::is_empty)
            || address.next().is_none_or(str::is_empty)
            || address.next().is_some()
        {
            return Err(invalid("from"));
        }
        let to = required("to")?;
        if to.chars().any(char::is_whitespace) {
            return Err(invalid("to"));
        }
        let mailbox = fields.get("mailbox").copied().unwrap_or("human");
        if !matches!(mailbox, "human" | "agent") {
            return Err(invalid("mailbox"));
        }
        let disposition = fields.get("disposition").copied().unwrap_or("keep");
        if !matches!(disposition, "keep" | "read" | "trash") {
            return Err(invalid("disposition"));
        }
        for field in ["subject", "body"] {
            if fields
                .get(field)
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(invalid(field));
            }
        }
        let priority = fields.get("priority").map_or(Ok(0), |value| {
            value.parse::<i64>().map_err(|_| invalid("priority"))
        })?;
        result.push(Rule {
            name: name.into(),
            mailbox: mailbox.into(),
            from: from.into(),
            subject: fields.get("subject").map(|value| (*value).into()),
            body: fields.get("body").map(|value| (*value).into()),
            to: to.into(),
            disposition: disposition.into(),
            priority,
        });
    }
    result.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(result)
}

/// Return the first matching route; callers use the ordering from `rules`.
pub fn choose<'a>(rules: &'a [Rule], mailbox: &str, parsed: &Parsed) -> Option<&'a Rule> {
    let subject = parsed.subject.to_lowercase();
    let body = parsed.body.to_lowercase();
    rules.iter().find(|rule| {
        rule.mailbox == mailbox
            && rule.from.eq_ignore_ascii_case(&parsed.sender)
            && rule
                .subject
                .as_ref()
                .is_none_or(|filter| subject.contains(&filter.to_lowercase()))
            && rule
                .body
                .as_ref()
                .is_none_or(|filter| body.contains(&filter.to_lowercase()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect()
    }

    #[test]
    fn overlapping_routes_use_priority_then_name() -> Result<(), String> {
        let settings = config(&[
            ("mail_route.z.from", "alerts@example.org"),
            ("mail_route.z.to", "general"),
            ("mail_route.b.from", "alerts@example.org"),
            ("mail_route.b.to", "later"),
            ("mail_route.b.priority", "2"),
            ("mail_route.a.from", "alerts@example.org"),
            ("mail_route.a.to", "first"),
            ("mail_route.a.priority", "2"),
        ]);
        let routes = rules(&settings)?;
        let parsed = Parsed {
            sender: "ALERTS@EXAMPLE.ORG".into(),
            ..Default::default()
        };
        assert_eq!(
            choose(&routes, "human", &parsed).map(|r| r.to.as_str()),
            Some("first")
        );
        assert!(choose(&routes, "agent", &parsed).is_none());
        assert_eq!(routes[0].disposition, "keep");
        Ok(())
    }

    #[test]
    fn general_filters_reload_without_retaining_old_rules() -> Result<(), String> {
        let mut settings = config(&[
            ("mail_route.build.from", "ci@example.org"),
            ("mail_route.build.to", "build_support"),
            ("mail_route.build.mailbox", "agent"),
            ("mail_route.build.subject", "Build failed"),
            ("mail_route.build.body", "service café"),
            ("mail_route.build.disposition", "read"),
        ]);
        let mut parsed = Parsed {
            sender: "CI@example.org".into(),
            subject: "Nightly BUILD FAILED".into(),
            body: "SERVICE CAFÉ needs repair".into(),
            ..Default::default()
        };
        assert!(choose(&rules(&settings)?, "agent", &parsed).is_some());
        parsed.sender = "other@example.org".into();
        assert!(choose(&rules(&settings)?, "agent", &parsed).is_none());
        parsed.sender = "ci@example.org".into();
        parsed.body.clear();
        assert!(choose(&rules(&settings)?, "agent", &parsed).is_none());
        parsed.body = "service café".into();
        settings.insert("mail_route.build.subject".into(), "success".into());
        assert!(choose(&rules(&settings)?, "agent", &parsed).is_none());
        settings.clear();
        assert!(rules(&settings)?.is_empty());
        Ok(())
    }

    #[test]
    fn malformed_routes_fail_instead_of_partially_routing() {
        for (field, value) in [
            ("unknown", "x"),
            ("mailbox", "both"),
            ("disposition", "delete"),
            ("priority", "1.2"),
            ("from", "Sender <a@b.org>"),
            ("from", "a@b@c"),
            ("to", ""),
            ("subject", ""),
            ("body", " "),
            ("to", "two tasks"),
        ] {
            let mut settings = config(&[
                ("mail_route.a.from", "a@b.org"),
                ("mail_route.a.to", "support"),
            ]);
            settings.insert(format!("mail_route.a.{field}"), value.into());
            assert!(rules(&settings).is_err(), "{field}={value}");
        }
        assert!(rules(&config(&[("mail_route.a", "true")])).is_err());
        assert!(rules(&config(&[("mail_route.a.to", "support")])).is_err());
    }
}

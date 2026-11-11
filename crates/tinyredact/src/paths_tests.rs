//! Home-path redaction behavior across platforms, profile names and text boundaries.

use super::*;

#[test]
fn openhuman_home_path_cases_map_to_home_marker() {
    for (input, expected) in [
        (
            "open /Users/oxoxdev/work/openhuman",
            "open <HOME>/work/openhuman",
        ),
        (r"C:\Users\oxoxdev\work\openhuman", r"<HOME>\work\openhuman"),
        ("C:/Users/oxoxdev/work/openhuman", "<HOME>/work/openhuman"),
        ("/home/jane/project", "<HOME>/project"),
        (r"c:\users\alice\work", r"<HOME>\work"),
        ("/HOME/alice/work", "<HOME>/work"),
        ("/Home/alice/work", "<HOME>/work"),
        (
            "from /Users/alice/a.txt to /Users/bob/b.txt",
            "from <HOME>/a.txt to <HOME>/b.txt",
        ),
        ("/Users/erin", "<HOME>"),
        (
            "open /home/carol/report.md and C:\\Users\\dave\\x.txt",
            "open <HOME>/report.md and <HOME>\\x.txt",
        ),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn tilde_and_unc_home_paths_are_scrubbed() {
    for (input, expected) in [
        ("~/Documents/report.txt", "<HOME>/Documents/report.txt"),
        ("~alice/Documents/report.txt", "<HOME>/Documents/report.txt"),
        (
            r"\\server\Users\alice\Documents\report.txt",
            r"<HOME>\Documents\report.txt",
        ),
        (r"\\server\home\alice\Documents", r"<HOME>\Documents"),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn unix_home_paths_in_empty_authority_file_uris_are_scrubbed() {
    for (input, expected) in [
        (
            "file:///Users/alice/Documents/report.txt",
            "file:///<HOME>/Documents/report.txt",
        ),
        ("file:///home/alice/a", "file:///<HOME>/a"),
        (
            "file:///Users/alice%20smith/My%20Documents/report.txt",
            "file:///<HOME>/My%20Documents/report.txt",
        ),
        (
            "FILE:///Users/alice/Documents/report.txt,",
            "FILE:///<HOME>/Documents/report.txt,",
        ),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn local_file_uri_platform_forms_preserve_uri_context() {
    for (input, expected) in [
        (
            "file://localhost/Users/alice/Documents/report.txt",
            "file://localhost/<HOME>/Documents/report.txt",
        ),
        (
            "file:///C:/Users/alice/Documents/report.txt",
            "file:///<HOME>/Documents/report.txt",
        ),
        (
            "file://localhost/C:/Users/alice/Documents/report.txt",
            "file://localhost/<HOME>/Documents/report.txt",
        ),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn file_uri_redaction_does_not_match_arbitrary_url_or_embedded_scheme_text() {
    for input in [
        "https://example.test/Users/alice/Documents/report.txt",
        "prefix-file:///Users/alice/Documents/report.txt",
        "file:///var/users/alice/Documents/report.txt",
    ] {
        assert_eq!(redact_home_paths(input), input);
    }
}

#[test]
fn public_file_uri_probe_matrix_preserves_context_and_redacts_home_paths() {
    for (input, expected) in [
        (
            "file:///Users/alice/Documents/report.txt",
            "file:///<HOME>/Documents/report.txt",
        ),
        ("file:///home/bob/a", "file:///<HOME>/a"),
        (
            "open file:///Users/alice/Documents/report.txt now",
            "open file:///<HOME>/Documents/report.txt now",
        ),
        (
            "\"file:///home/alice/report.txt\"",
            "\"file:///<HOME>/report.txt\"",
        ),
        (
            "[report](file:///Users/alice/Documents/report.txt)",
            "[report](file:///<HOME>/Documents/report.txt)",
        ),
        (
            "📁 file:///home/用户/文書/report.txt",
            "📁 file:///<HOME>/文書/report.txt",
        ),
        (
            "file:///Users/alice/a to file:///home/bob/b",
            "file:///<HOME>/a to file:///<HOME>/b",
        ),
        (
            "file://server/Users/alice/Documents/report.txt",
            "file://server/<HOME>/Documents/report.txt",
        ),
        (
            "file://server/home/alice/report.txt",
            "file://server/<HOME>/report.txt",
        ),
        (
            "file://localhost/Users/alice/report.txt",
            "file://localhost/<HOME>/report.txt",
        ),
        (
            "open file://localhost/Users/alice/report.txt",
            "open file://localhost/<HOME>/report.txt",
        ),
        (
            "file:///C:/Users/alice/report.txt",
            "file:///<HOME>/report.txt",
        ),
        (
            "open file:///C:/Users/alice/report.txt",
            "open file:///<HOME>/report.txt",
        ),
        (
            "https://example.test/Users/alice/report.txt",
            "https://example.test/Users/alice/report.txt",
        ),
        (
            "open https://example.test/Users/alice/report.txt",
            "open https://example.test/Users/alice/report.txt",
        ),
        (
            "\"https://example.test/home/alice/report.txt\"",
            "\"https://example.test/home/alice/report.txt\"",
        ),
        (
            "prefix-file:///Users/alice/a",
            "prefix-file:///Users/alice/a",
        ),
        (
            "file:///Users/alice%20smith/My%20Documents/a%2Fb.txt",
            "file:///<HOME>/My%20Documents/a%2Fb.txt",
        ),
        (
            "file:///home/Zo\u{0308}eSmith/文書/報告.txt",
            "file:///<HOME>/文書/報告.txt",
        ),
        (
            "file:///Users/O'Connor/Documents",
            "file:///<HOME>/Documents",
        ),
        ("file:///home/alice, done", "file:///<HOME>, done"),
        ("//server/Users/alice/Documents", "<HOME>/Documents"),
        (
            "file:///var/users/alice/Documents",
            "file:///var/users/alice/Documents",
        ),
        (
            "hello 世界 /var/log/messages",
            "hello 世界 /var/log/messages",
        ),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn adjacent_uri_and_path_text_keeps_each_redaction_boundary() {
    for (input, expected) in [
        (
            r#""https://example.test/a","/home/bob/b""#,
            r#""https://example.test/a","<HOME>/b""#,
        ),
        (
            r#""file:///Users/alice/a","file:///home/bob/b""#,
            r#""file:///<HOME>/a","file:///<HOME>/b""#,
        ),
        (
            r#"[site](https://example.test/a),[file](file:///Users/alice/b)"#,
            r#"[site](https://example.test/a),[file](file:///<HOME>/b)"#,
        ),
        (
            r#""file:///Users/alice/a","//server/home/bob/b""#,
            r#""file:///<HOME>/a","<HOME>/b""#,
        ),
        (
            "(file:///home/alice),(/Users/bob)",
            "(file:///<HOME>),(<HOME>)",
        ),
        (
            r#""file:///var/log/a","/Users/alice/b""#,
            r#""file:///var/log/a","<HOME>/b""#,
        ),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn embedded_uri_schemes_follow_word_boundaries_and_suffixes_survive() {
    for (input, expected) in [
        ("项目file:///Users/alice/a", "项目file:///Users/alice/a"),
        (
            "prefix_file:///Users/alice/a",
            "prefix_file:///Users/alice/a",
        ),
        (
            "prefix-file:///Users/alice/a",
            "prefix-file:///Users/alice/a",
        ),
        (
            "file:///home/alice?download=true",
            "file:///<HOME>?download=true",
        ),
        ("file:///Users/alice#section", "file:///<HOME>#section"),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn uri_path_punctuation_stays_inside_profiles_and_non_file_resources() {
    for (input, expected) in [
        (
            "file://server/home/alice;smith/Documents",
            "file://server/<HOME>/Documents",
        ),
        (
            "file://server/Users/alice)smith/Documents",
            "file://server/<HOME>/Documents",
        ),
        (
            "file:///Users/alice,smith/Documents",
            "file:///<HOME>/Documents",
        ),
        (
            "https://example.test/a,/home/alice/report.txt",
            "https://example.test/a,/home/alice/report.txt",
        ),
        (
            "https://example.test/a;/home/alice/report.txt",
            "https://example.test/a;/home/alice/report.txt",
        ),
        (
            "https://example.test/a)/home/alice/report.txt",
            "https://example.test/a)/home/alice/report.txt",
        ),
        ("file:///~/Documents", "file:///~/Documents"),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn empty_file_uri_roots_are_unchanged_without_panicking() {
    for input in [
        "file:///",
        "file://server/",
        "file://localhost/",
        "file:///?q=a",
        "file:///C:/",
        "file:///Users/",
        "file:///home/",
    ] {
        assert_eq!(redact_home_paths(input), input, "input={input:?}");
    }
}

#[test]
fn matched_quote_context_resumes_redaction_after_each_uri() {
    for (input, expected) in [
        (
            r#"'https://example.test/a','/home/bob/b'"#,
            r#"'https://example.test/a','<HOME>/b'"#,
        ),
        (
            r#"'file:///Users/alice/a','file:///home/bob/b'"#,
            r#"'file:///<HOME>/a','file:///<HOME>/b'"#,
        ),
        (
            r#"'file://server/Users/alice/a','/home/bob/b'"#,
            r#"'file://server/<HOME>/a','<HOME>/b'"#,
        ),
        (
            r#"'file:///Users/O'Connor/Documents'"#,
            r#"'file:///<HOME>/Documents'"#,
        ),
        (
            "`https://example.test/a`,`/Users/bob/b`",
            "`https://example.test/a`,`<HOME>/b`",
        ),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn balanced_uri_wrappers_do_not_truncate_parenthesized_profiles() {
    for (input, expected) in [
        (
            "(file://server/home/alice(foo),smith/Documents)",
            "(file://server/<HOME>/Documents)",
        ),
        (
            "(file://server/home/alice(foo(bar),baz),smith/Documents)",
            "(file://server/<HOME>/Documents)",
        ),
        (
            "file:///Users/alice!$&'()*+,;=:@-._~%20/Documents",
            "file:///<HOME>/Documents",
        ),
        (
            "[file](file:///home/alice(foo),smith/Documents),(/Users/bob)",
            "[file](file:///<HOME>/Documents),(<HOME>)",
        ),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn every_quote_transition_keeps_adjacent_home_tokens_visible_to_redaction() {
    let quote_types = ['\'', '"', '`'];
    let separators = [',', ';'];
    let following_paths = [
        ("/home/bob/b", "<HOME>/b"),
        ("file:///Users/bob/b", "file:///<HOME>/b"),
        ("//server/home/bob/b", "<HOME>/b"),
    ];

    for first_quote in quote_types {
        for next_quote in quote_types {
            for separator in separators {
                for (path, redacted_path) in following_paths {
                    let input = format!(
                        "{first_quote}https://example.test/a{first_quote}{separator}{next_quote}{path}{next_quote}"
                    );
                    let expected = format!(
                        "{first_quote}https://example.test/a{first_quote}{separator}{next_quote}{redacted_path}{next_quote}"
                    );
                    assert_eq!(redact_home_paths(&input), expected, "input={input:?}");
                }
            }
        }
    }
}

#[test]
fn quoted_uris_end_before_unquoted_home_tokens_after_separators() {
    let quote_types = ['\'', '"', '`'];
    let separators = [',', ';'];
    let following_paths = [
        ("/home/bob/b", "<HOME>/b"),
        ("file:///Users/bob/b", "file:///<HOME>/b"),
        ("//server/home/bob/b", "<HOME>/b"),
        (r"C:\Users\bob\docs", r"<HOME>\docs"),
    ];

    for quote in quote_types {
        for separator in separators {
            for (path, redacted_path) in following_paths {
                let input = format!("{quote}https://example.test/a{quote}{separator}{path}");
                let expected =
                    format!("{quote}https://example.test/a{quote}{separator}{redacted_path}");
                assert_eq!(redact_home_paths(&input), expected, "input={input:?}");
            }
        }
    }
}

#[test]
fn punctuation_inside_quoted_profiles_survives_until_the_descendant_separator() {
    for (input, expected) in [
        (
            "'file://server/Users/O'.Connor/Documents'",
            "'file://server/<HOME>/Documents'",
        ),
        (
            "'file://server/Users/alice')smith/Documents'",
            "'file://server/<HOME>/Documents'",
        ),
        (
            "(file://server/home/alice),smith/Documents)",
            "(file://server/<HOME>/Documents)",
        ),
        (
            "file://server/Users/alice]smith/Documents",
            "file://server/<HOME>/Documents",
        ),
        (
            "'https://example.test/Users/O'.Connor/Documents'",
            "'https://example.test/Users/O'.Connor/Documents'",
        ),
        (
            "'https://example.test/Users/alice')smith/Documents'",
            "'https://example.test/Users/alice')smith/Documents'",
        ),
    ] {
        assert_eq!(redact_home_paths(input), expected, "input={input:?}");
    }
}

#[test]
fn unrelated_absolute_paths_and_non_paths_are_unchanged() {
    for input in [
        "/var/log/openhuman.log",
        "/opt/users/alice/data",
        r"D:\Projects\openhuman",
        "hello world",
        "prefix/home/alice/file",
        "项目/home/alice/file",
        "/home/",
        "/Users/",
        r"\server\Users\alice\data",
    ] {
        assert_eq!(redact_home_paths(input), input);
    }
}

#[test]
fn unicode_and_path_boundaries_are_preserved() {
    assert_eq!(
        redact_home_paths("📁 /Users/álîçé/文書/報告.txt and /home/用户/项目"),
        "📁 <HOME>/文書/報告.txt and <HOME>/项目"
    );
    assert_eq!(
        redact_home_paths("x=/home/a/one; y=~/two"),
        "x=<HOME>/one; y=<HOME>/two"
    );
}

#[test]
fn prose_after_bare_unix_windows_and_unc_homes_is_preserved() {
    assert_home_path_cases(&[
        ("open /Users/alice now", "open <HOME> now"),
        ("/home/alice and /home/bob/docs", "<HOME> and <HOME>/docs"),
        (
            r"C:\Users\alice and D:\Projects\report.txt",
            r"<HOME> and D:\Projects\report.txt",
        ),
        (r"\\server\Users\alice and more", r"<HOME> and more"),
    ]);
}

#[test]
fn punctuation_around_bare_home_paths_is_preserved() {
    assert_home_path_cases(&[
        ("(/Users/alice)", "(<HOME>)"),
        ("'/home/alice'", "'<HOME>'"),
        (r#""C:\Users\alice""#, r#""<HOME>""#),
        (r"\\server\Users\alice)", r"<HOME>)"),
        ("/Users/alice.", "<HOME>."),
        (
            "from /home/alice,/home/bob; done",
            "from <HOME>,<HOME>; done",
        ),
    ]);
}

#[test]
fn tilde_home_tokens_keep_boundaries_and_ignore_relative_dot_path() {
    assert_home_path_cases(&[
        ("~", "<HOME>"),
        ("~/docs", "<HOME>/docs"),
        ("~alice now", "<HOME> now"),
        ("~alice.smith/notes", "<HOME>/notes"),
        ("~alice.", "<HOME>."),
        ("(~)", "(<HOME>)"),
        ("~.", "~."),
        ("~.config/settings", "~.config/settings"),
    ]);
}

#[test]
fn dots_and_hyphens_inside_username_tokens_are_scrubbed() {
    assert_home_path_cases(&[
        (
            "/Users/alice.smith/docs and /home/bob-jones/work",
            "<HOME>/docs and <HOME>/work",
        ),
        (
            r"C:\Users\alice.smith\docs and \\server\Users\bob-jones\work",
            r"<HOME>\docs and <HOME>\work",
        ),
    ]);
}

#[test]
fn quoted_windows_and_unc_profiles_with_spaces_are_fully_scrubbed() {
    assert_home_path_cases(&[
        (
            r#""C:\Users\Jane Doe\Documents\report.txt""#,
            r#""<HOME>\Documents\report.txt""#,
        ),
        (
            r#""C:\Users\Mary-Jane Smith\Documents""#,
            r#""<HOME>\Documents""#,
        ),
        (r#"'C:\Users\Jane Doe'"#, r#"'<HOME>'"#),
        (
            r#""\\server\Users\Jane Doe\Documents""#,
            r#""<HOME>\Documents""#,
        ),
        (r#"'\\server\Users\Jane Doe'"#, r#"'<HOME>'"#),
        (
            r#""D:\Projects\Jane Doe\report.txt""#,
            r#""D:\Projects\Jane Doe\report.txt""#,
        ),
    ]);
}

#[test]
fn unquoted_windows_profiles_with_spaces_are_bounded_by_their_descendants() {
    assert_home_path_cases(&[
        (r"C:\Users\Jane Doe\Documents", r"<HOME>\Documents"),
        (r"\\server\Users\Jane Doe\Documents", r"<HOME>\Documents"),
        (r#""C:\Users\Jane Doe\Documents""#, r#""<HOME>\Documents""#),
        (
            r#""\\server\Users\Jane Doe\Documents""#,
            r#""<HOME>\Documents""#,
        ),
        (
            r"open C:\Users\alice and D:\Projects\report.txt",
            r"open <HOME> and D:\Projects\report.txt",
        ),
        (
            r"C:\Users\Jane Doe\Documents and /home/bob/docs",
            r"<HOME>\Documents and <HOME>/docs",
        ),
        (
            r"\\server\Users\Jane Doe\Documents and \\other\Users\Bob\docs",
            r"<HOME>\Documents and <HOME>\docs",
        ),
    ]);
}

#[test]
fn legacy_home_components_with_symbols_are_redacted_through_boundaries() {
    assert_home_path_cases(&[
        ("/Users/O'Connor/Documents", "<HOME>/Documents"),
        ("/home/alice+dev/work", "<HOME>/work"),
        ("/Users/service$/Documents", "<HOME>/Documents"),
        ("/Users/alice\u{200d}dev/Documents", "<HOME>/Documents"),
        (r#""C:\Users\O'Connor\Documents""#, r#""<HOME>\Documents""#),
        (r"C:\Users\alice+dev\Documents", r"<HOME>\Documents"),
        (r"C:\Users\service$\Documents", r"<HOME>\Documents"),
        ("/home/$service/work", "<HOME>/work"),
        ("/home/$service", "<HOME>"),
        ("/Users/alice+dev", "<HOME>"),
        ("open /Users/O'Connor now", "open <HOME> now"),
        ("/Users/O'Connor!", "<HOME>!"),
        ("/home/alice+dev?", "<HOME>?"),
    ]);
}

#[test]
fn decomposed_unicode_username_marks_and_following_letters_are_scrubbed() {
    assert_home_path_cases(&[
        ("/Users/Zo\u{0308}eSmith/Documents", "<HOME>/Documents"),
        ("/Users/Jose\u{0301}/Documents", "<HOME>/Documents"),
        ("/home/Zo\u{0308}eSmith/work", "<HOME>/work"),
        ("/home/क्षमा/work", "<HOME>/work"),
    ]);
}

#[test]
fn a_home_path_inside_a_uri_query_or_fragment_is_redacted() {
    assert_home_path_cases(&[
        (
            "https://example.com/open?path=/Users/alice/secret.txt",
            "https://example.com/open?path=<HOME>/secret.txt",
        ),
        (
            "file:///tmp/x?ref=/home/alice/notes",
            "file:///tmp/x?ref=<HOME>/notes",
        ),
        (
            "(https://x.test/?f=/Users/alice/a)",
            "(https://x.test/?f=<HOME>/a)",
        ),
        (
            "https://x.test/doc#/home/alice/notes",
            "https://x.test/doc#<HOME>/notes",
        ),
    ]);
}

fn assert_home_path_cases(cases: &[(&str, &str)]) {
    let mismatches = cases
        .iter()
        .filter_map(|(input, expected)| {
            let actual = redact_home_paths(input);
            (actual != *expected)
                .then(|| format!("input={input:?}, actual={actual:?}, expected={expected:?}"))
        })
        .collect::<Vec<_>>();
    assert!(
        mismatches.is_empty(),
        "home path mismatches:\n{}",
        mismatches.join("\n")
    );
}

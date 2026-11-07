//! The redaction corpus: every secret shape the capture path promises to mask.
//!
//! Each case goes through the real [`Parser`] rather than `redact_str`, because the bugs this
//! guards against lived between the two: a regex that passes on one buffer can still fail on
//! the lines the pipeline actually feeds it. Secrets are assembled at runtime so that the
//! file itself never trips a secret scanner.

use logbear_core::parse::Parser;
use logbear_core::redact::Redactor;
use std::sync::Arc;

struct Case {
    name: &'static str,
    /// What the process wrote. Newlines included, since some shapes span lines.
    input: String,
    /// Substrings that must not survive anywhere.
    secrets: Vec<String>,
}

fn case(name: &'static str, input: impl Into<String>, secrets: &[&str]) -> Case {
    Case {
        name,
        input: input.into(),
        secrets: secrets.iter().map(|s| s.to_string()).collect(),
    }
}

fn aws() -> String {
    format!("AKIA{}", "IOSFODNN7EXAMPLE")
}
fn github() -> String {
    format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123456789")
}
fn fine_grained() -> String {
    format!("github_pat_{}_{}", "11ABCDEFG0abcdefghijkl", "x".repeat(40))
}
fn google() -> String {
    format!("AIza{}", "SyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q")
}
fn npm() -> String {
    format!("npm_{}", "aB3".repeat(12))
}
fn hugging_face() -> String {
    format!("hf_{}", "aB3".repeat(11))
}
fn slack_app() -> String {
    format!("xapp-1-A0123456789-{}", "1234567890123-abcdef0123456789")
}

const PEM_BODY: [&str; 3] = [
    "MIIEowIBAAKCAQEAsecretbodylineone1234567890abcdefghijklmnop",
    "c2Vjb25kbGluZW9mdGhla2V5Ym9keXRoYXRtdXN0bm90bGFuZHRoaXNvbmU=",
    "dGhpcmRsaW5lb2Z0aGVrZXk=",
];

fn pem(kind: &str, eol: &str) -> String {
    let mut s = format!("-----BEGIN {kind}-----{eol}");
    for line in PEM_BODY {
        s.push_str(line);
        s.push_str(eol);
    }
    s.push_str(&format!("-----END {kind}-----{eol}"));
    s
}

fn corpus() -> Vec<Case> {
    let (aws, github, pat, google, npm, hf, xapp) = (
        aws(),
        github(),
        fine_grained(),
        google(),
        npm(),
        hugging_face(),
        slack_app(),
    );
    vec![
        // P2: multi-line PEM bodies.
        case("pem rsa", pem("RSA PRIVATE KEY", "\n"), &PEM_BODY),
        case("pem pkcs8", pem("PRIVATE KEY", "\r\n"), &PEM_BODY),
        case("pem encrypted", pem("ENCRYPTED PRIVATE KEY", "\n"), &PEM_BODY),
        case("pem openssh", pem("OPENSSH PRIVATE KEY", "\n"), &PEM_BODY),
        case("pem pgp block", pem("PGP PRIVATE KEY BLOCK", "\n"), &PEM_BODY),
        case(
            "pem inside a log prefix",
            format!("[info] key is {}", pem("PRIVATE KEY", "\n")),
            &PEM_BODY,
        ),
        case(
            "pem in colour",
            format!("\x1b[31m{}\x1b[0m\n", pem("PRIVATE KEY", "\n")),
            &PEM_BODY,
        ),
        // P3: secrets wrapped in, or broken up by, colour.
        case("sgr before", format!("key \x1b[33m{aws}\x1b[0m\n"), &[&aws]),
        case("sgr 256 colour", format!("\x1b[38;5;208m{github}\x1b[39m\n"), &[&github]),
        case("sgr truecolor", format!("\x1b[38;2;1;2;3m{npm}\x1b[0m ok\n"), &[&npm]),
        case(
            "sgr split inside a value",
            "password=\x1b[33mhunter2\x1b[0mhunter3 done\n",
            &["hunter2", "hunter3"],
        ),
        case(
            "key and value coloured apart",
            "\x1b[36mpassword\x1b[0m: \x1b[32m'hunter2'\x1b[39m\n",
            &["hunter2"],
        ),
        case(
            "token split by a reset",
            format!("{}\x1b[0m{}\n", &aws[..8], &aws[8..]),
            &[&aws[8..]],
        ),
        case(
            "osc 8 hyperlink target",
            "\x1b]8;;postgres://admin:s3cr3tpw@db/app\x1b\\db\x1b]8;;\x1b\\\n",
            &["s3cr3tpw"],
        ),
        // P5: common shapes.
        case("github fine-grained", format!("token {pat} end\n"), &[&pat]),
        case("json password", "{\"user\":\"a\",\"password\":\"hunter2\"}\n", &["hunter2"]),
        case("json spaced", "{ \"api_key\" : \"abc def 123\" }\n", &["abc def 123"]),
        case(
            "json escaped in json",
            "{\"body\":\"{\\\"password\\\":\\\"hunter2\\\"}\"}\n",
            &["hunter2"],
        ),
        case("json client secret", "{\"clientSecret\":\"hunter2\"}\n", &["hunter2"]),
        case("google", format!("maps key={google}\n"), &[&google]),
        case("google bare", format!("using {google} for maps\n"), &[&google]),
        case("npm", format!("//registry/:_x={npm}\n"), &[&npm]),
        case("hugging face", format!("model token {hf}\n"), &[&hf]),
        case("slack app", format!("connecting with {xapp}\n"), &[&xapp]),
        case("redis password only", "REDIS=redis://:s3cr3tpw@cache:6379/0\n", &["s3cr3tpw"]),
        case("cookie", "Cookie: session=abc123; theme=dark\n", &["abc123", "theme=dark"]),
        case(
            "set-cookie",
            "set-cookie: sid=abc123; Path=/; HttpOnly\n",
            &["abc123", "HttpOnly"],
        ),
        case("x-api-key", "X-Api-Key: abc123def\n", &["abc123def"]),
        case("db pass", "DB_PASS=hunter2\n", &["hunter2"]),
        case("mysql pass", "MYSQL_PASS: 'hunter2'\n", &["hunter2"]),
        // The shapes that were already handled, so a rewrite cannot quietly lose them.
        case("aws", format!("key {aws} here\n"), &[&aws]),
        case("github classic", format!("{github}\n"), &[&github]),
        case(
            "authorization",
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456\n",
            &["abcdefghijklmnopqrstuvwxyz123456"],
        ),
        case("connection string", "postgres://admin:s3cr3tpw@db:5432/app\n", &["s3cr3tpw"]),
        case("key equals value", "export API_SECRET=hunter2\n", &["hunter2"]),
    ]
}

/// Runs a stream through a redacting parser in the given pieces, returning the stored line
/// bytes and every entry's text and template.
fn capture(pieces: &[&[u8]]) -> (Vec<Vec<u8>>, String) {
    let mut p = Parser::new().with_redactor(Arc::new(Redactor::new(true, &[])));
    let (mut lines, mut text) = (Vec::new(), String::new());
    let mut take = |out: logbear_core::parse::ParseOutput| {
        lines.extend(out.lines.into_iter().map(|l| l.bytes));
        for e in &out.entries {
            text.push_str(&e.text);
            text.push('\n');
            text.push_str(&e.template);
            text.push('\n');
        }
    };
    for (i, piece) in pieces.iter().enumerate() {
        take(p.push(piece, i as i64));
    }
    take(p.finish(pieces.len() as i64));
    (lines, text)
}

/// Every CSI sequence in a stream, in order.
fn escapes(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let mut e = i + 2;
            while e < bytes.len() && !(0x40..=0x7e).contains(&bytes[e]) {
                e += 1;
            }
            out.push(bytes[i..=e.min(bytes.len() - 1)].to_vec());
            i = e;
        }
        i += 1;
    }
    out
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn check(c: &Case, pieces: &[&[u8]], how: &str) {
    let (lines, text) = capture(pieces);
    let stored: Vec<u8> = lines.join(&b'\n');
    for s in &c.secrets {
        assert!(
            !contains(&stored, s),
            "{} ({how}): {s:?} survived in stored bytes: {:?}",
            c.name,
            String::from_utf8_lossy(&stored)
        );
        assert!(
            !text.contains(s.as_str()),
            "{} ({how}): {s:?} survived in entry text: {text}",
            c.name
        );
    }

    // Byte-exact replay: the same bytes minus the transport's trailing CR, in the same places.
    let plain: Vec<u8> = c.input.replace("\r\n", "\n").into_bytes();
    let want = plain.strip_suffix(b"\n").unwrap_or(&plain);
    assert_eq!(stored.len(), want.len(), "{} ({how}): length drifted", c.name);
    assert_eq!(escapes(&stored), escapes(want), "{} ({how}): escapes were disturbed", c.name);
}

#[test]
fn every_shape_is_masked_whole_at_length_with_escapes_intact() {
    for c in corpus() {
        check(&c, &[c.input.as_bytes()], "one read");
    }
}

#[test]
fn every_shape_is_masked_when_split_at_any_read_boundary() {
    for c in corpus() {
        let bytes = c.input.as_bytes();
        for at in 1..bytes.len() {
            check(&c, &[&bytes[..at], &bytes[at..]], &format!("split at {at}"));
        }
    }
}

#[test]
fn every_shape_is_masked_one_byte_at_a_time() {
    for c in corpus() {
        let pieces: Vec<&[u8]> = c.input.as_bytes().chunks(1).collect();
        check(&c, &pieces, "byte by byte");
    }
}

#[test]
fn an_unterminated_pem_stops_masking_after_its_bound() {
    let mut stream = String::from("-----BEGIN PRIVATE KEY-----\n");
    for i in 0..500 {
        stream.push_str(&format!("line{i}visible\n"));
    }
    let (lines, _) = capture(&[stream.as_bytes()]);
    assert!(!contains(&lines[1], "line0"), "the body right after BEGIN must be masked");
    assert!(
        !contains(&lines[199], "line198"),
        "the block should stay masked inside the bound"
    );
    assert!(
        contains(lines.last().unwrap(), "line499"),
        "masking must end: an unterminated BEGIN cannot swallow the rest of the run"
    );
}

#[test]
fn text_after_a_pem_block_is_left_alone() {
    let stream = format!("{}server listening on 4000\n", pem("PRIVATE KEY", "\n"));
    let (lines, _) = capture(&[stream.as_bytes()]);
    assert_eq!(lines.last().unwrap(), b"server listening on 4000");
}

#[test]
fn ordinary_lines_are_untouched() {
    for line in [
        "12:04:11.882 [info] GET /users",
        "** (Ecto.NoResultsError) expected at least one result",
        "    at Object.<anonymous> (/app/server.js:12:9)",
        "compiled 41 files in 2.3s",
        "PASS: src/app.test.js",
        "pass: 12 tests",
        "\x1b[32m12:04:11 [info] all good\x1b[0m",
        "npm_config_registry=https://registry.npmjs.org/",
        "http://localhost:4000/users/42",
        "{\"input_tokens\":1500,\"level\":\"info\"}",
        "{\"author\":\"suki\"}",
    ] {
        let (lines, _) = capture(&[line.as_bytes(), b"\n"]);
        assert_eq!(lines[0], line.as_bytes(), "false positive on: {line}");
    }
}

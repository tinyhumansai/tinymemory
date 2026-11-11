# `safety` — secret and PII scrubbing

`tinyredact` removes credentials and
personal identifiers from text before a memory host stores it. It is
conservative by design: it would rather redact a harmless string than let a
token or a national ID into a long-lived store. It runs on-device, uses regular
expressions and checksums only, and makes no network calls.

The usual call is [`scrub_item`] on each `StoreItem` just before
`MemoryEngine::store`.

Nothing calls it for you: no engine scrubs on its own, so the host runs it in
the write pipeline (source, documents, safety, engine). See
[`docs/architecture/integrations.md`](../../../../docs/architecture/integrations.md).

## Layout

```text
safety/
├── mod.rs          # module docs, `mod` declarations, public re-exports
├── policy/         # Policy, BareCardGate, SanitizationReport, Sanitized
├── sanitize/       # sanitize_text[_with], sanitize_json[_with], has_likely_secret
│   └── patterns.rs # private-key block and credential shape tables
├── markers/        # redact_credential_markers: `/secret/<key>`, `Bearer <value>`
├── pii/            # redact_pii[_with], has_likely_pii, has_likely_email
│   ├── prefilter.rs  # byte pass deciding which identifier classes run
│   ├── normalize.rs  # fullwidth / zero-width / Arabic-Indic folding
│   └── checks.rs     # Luhn, mod-97, Verhoeff, CPF/CNPJ/CUIT, DNI/NIE, SSN, NINO
├── item/           # scrub_item[_with]: applies the scrubber to a StoreItem
└── pattern/        # literal(): the one place a built-in regex is compiled
```

## Public surface

| Item | Purpose |
| --- | --- |
| `scrub_item`, `scrub_item_with` | Scrub every free text a `StoreItem` carries |
| `sanitize_text`, `sanitize_text_with` | Scrub a string: secrets, then PII |
| `sanitize_json`, `sanitize_json_with` | Scrub a JSON value recursively |
| `has_likely_secret` | Boolean check: does this text look like it holds a credential |
| `has_likely_pii` | Strict boundary check used to *reject* namespaces and keys |
| `has_likely_email` | Boundary check for an ordinary email address |
| `redact_credential_markers` | Only the marker rules, without the PII pass |
| `redact_home_paths` | Replace recognized home-directory prefixes with `<HOME>` |
| `pii::redact_pii`, `pii::redact_pii_with` | Only the PII pass |
| `Policy`, `BareCardGate` | The single tunable |
| `Sanitized<T>`, `SanitizationReport` | Cleaned value plus a tally of changes |

The sanitization functions return a `Sanitized<T>`;
`report.changed()` says whether anything was replaced. The standalone
`redact_home_paths` rule returns the scrubbed string directly.

`redact_home_paths` is a standalone text rule for `/home/<user>`,
`/Users/<user>`, drive-qualified and UNC Windows paths, and `~` home paths.
It also recognizes file URIs with empty, `localhost`, or remote authorities,
redacting a home profile at the path root while preserving the scheme,
authority, and remaining path. Windows drive paths such as
`file:///C:/Users/...` are supported. URI path text is scanned as written;
percent escapes are not decoded.
It preserves text below the home directory and leaves unrelated absolute paths
alone; call it separately when a host needs this rule without the full secret
and PII pipeline. After an explicit home prefix, the nonempty profile component
is redacted through its path separator, including arbitrary symbols and UTF-8.
Without a descendant separator, whitespace and clear closing punctuation bound
the bare component; terminal punctuation remains in the text. Bare `~` and
`~<user>` are supported, while `~.`
and `~.<name>` remain unchanged as relative paths. Windows drive and UNC
profiles may contain spaces when a descendant separator completes the same
path; a new absolute path after prose ends the profile. Quoted paths use the
matching quote to bound a bare profile, and the separator to bound descendants.

## What is blocked and what is redacted

A text pass in `sanitize_text_with` runs four stages, in this order:

1. **Private-key blocks are blocked.** A PEM, OpenSSH or PGP private-key block
   is replaced in full by `[REDACTED_PRIVATE_KEY]` and counted in
   `blocked_secret_hits`. Nothing of the block survives.
2. **Credential markers.** The value after `/secret/` in a one-time-secret URL,
   and after a `Bearer ` scheme, becomes `[REDACTED]`. The marker and the
   surrounding prose stay, so memory still records that a link or token was
   shared (see below).
3. **Credential shapes are redacted.** Provider token prefixes (`sk-`,
   `sk-ant-`, `ghp_`, `github_pat_`, `glpat-`, `xox?-`, `AKIA`/`ASIA`, `AIza`,
   `npm_`, `SG.`, Stripe `sk_live_`/`rk_test_` and so on), JWTs, and
   `key=value` assignments whose key is `api_key`, `token`, `password`,
   `secret`, `client_secret` or an OAuth parameter. The matched span becomes
   `[REDACTED]`; for `Bearer` and `api_key` the prefix is kept. Counted in
   `text_redactions`.
4. **PII is redacted.** The `pii` pass replaces each identifier with a typed
   token such as `[REDACTED_PII_CPF]` or `[REDACTED_PII_CREDIT_CARD]`. Counted
   in `pii_redactions`.

`sanitize_json_with` walks objects and arrays:

- A value whose **key** looks sensitive is replaced by `[REDACTED_SECRET]`
  without being read. Keys are compared lowercased with non-alphanumerics
  removed, so `API-Key`, `api_key` and `apiKey` match alike. Exact names
  (`apikey`, `token`, `authorization`, `password`, `secret`, `clientsecret`, …)
  match, as does any key that ends in `token`, `apikey`, `clientsecret` or
  `key`, or contains `password` or `secret`. Counted in `key_redactions`.
- Every other string runs through `sanitize_text_with`.
- Numbers, booleans and null pass through untouched.
- Nesting deeper than 128 levels is not walked: the subtree is replaced by
  `[REDACTED_SECRET]` and counted in `depth_redactions`.

The boolean checks are separate from scrubbing. `has_likely_secret` tests the
block and shape tables (not the markers). `has_likely_pii` uses a stricter
pattern set than content scrubbing; see the PII section.

## The `Policy` knob

`Policy` has one field, `bare_card: BareCardGate`. It decides how a bare
(separator-free) Luhn-valid 13-19 digit run is judged as a credit card:

- `BareCardGate::LuhnOnly` (the default) redacts every Luhn-valid run. This is
  the strictest setting and what the plain functions use.
- `BareCardGate::Corroborated` additionally requires a real network IIN at an
  issued length, or a card keyword within 64 bytes (`card`, `cc`, `pan`,
  `cardNumber`, `信用卡`, `カード`, …). `Policy::corroborated()` builds it.

Separated runs (`4111 1111 1111 1111`) are Luhn-gated under both settings. The
corroborated gate exists because Luhn passes about one in ten arbitrary digit
runs, and 13-digit epoch-millisecond timestamps in stored JSON envelopes were
being corrupted at that rate (opencompany#1201). A host scrubbing items that
carry such timestamps should opt in; a caller that does not never redacts less
than before.

## PII detection pipeline

`pii::redact_pii_with` runs in three steps.

1. **Normalize.** `NormalizedView` builds a copy of the text with zero-width
   characters (U+200B/200C/200D/FEFF/2060/180E) removed, fullwidth digits and
   `．－／：` folded to ASCII, and Arabic-Indic digits folded to ASCII. It keeps
   a byte map back to the original, so `１１１.４４４…` or a digit run with
   zero-width spaces inside cannot slip past.
2. **Prefilter.** `scan_candidates` makes one cheap pass over the bytes and
   sets a flag per identifier class from structural signals: digit-run
   lengths, punctuation, letters, `+`, and keyword probes. Every flag is a
   necessary condition of its class's precise regex, so it can over-fire but
   never under-fire. A class whose flag is unset is skipped, and its regex is
   never compiled.
3. **Match and check.** The precise regex of each flagged class runs on the
   normalized text, in priority order, and each candidate passes its checksum
   or structural gate:

   | Class | Gate |
   | --- | --- |
   | Brazil CPF / CNPJ (formatted or bare) | mod-11 check digits |
   | Argentina CUIT/CUIL (formatted only) | check digit |
   | Credit card | Luhn, plus `Policy` for bare runs |
   | IBAN | mod-97 |
   | India Aadhaar | Verhoeff when grouped; keyword when bare |
   | Spain DNI / NIE | check letter |
   | US SSN | reserved-range filters |
   | UK NINO | reserved-prefix filters |
   | Japan My Number | keyword nearby |
   | Mexico RFC, India PAN, Korea RRN | format only |
   | Phone: E.164, NANP | format (NANP area/exchange rules) |

Overlapping hits are resolved earliest-and-longest first, so a card number is
not also partly redacted as a phone number. The kept hits are spliced back onto
the **original** bytes through the byte map; text that is not PII, including
fullwidth glyphs a user typed on purpose, is left exactly as it was.

### The strict boundary check

`has_likely_pii` decides whether to *reject* a namespace or key, not whether to
rewrite content. It runs the same pipeline but leaves out the patterns whose
only signal is a digit-run shape: bare credit cards, bare CPF/CNPJ, NANP and
E.164 phones. Scanner-built identifiers (WhatsApp JIDs such as
`12025551234-1543890267@g.us`, Telegram peer IDs, millisecond timestamps,
padded counters) would otherwise be rejected constantly. Formatted national IDs
are still rejected. `has_likely_email` is kept apart for the same reason:
identifiers can contain email-like `@` segments legitimately.

## Credential markers

Two leaks get past shape-based matching. Both were seen in a live OpenCompany
deployment that remembered every operator message verbatim:

- **One-time-secret URLs** such as `https://ots.example/secret/<key>`. The key
  is the credential, and it doesn't look like one. The value after `/secret/`
  is redacted; the match is exact and lowercase.
- **Short `Bearer` values.** `Bearer s3cret` is a valid credential, but the
  shape regex needs eight characters. The `Bearer` rule matches the scheme
  case-insensitively and is tuned against the English word, so "ring bearer"
  and "bearer bond" are left alone.

Only the value after the marker is replaced. `redact_credential_markers` runs
just these two rules and returns the input borrowed, without allocating, when
neither marker is present. `sanitize_text` runs them before the shape regexes,
and its `[REDACTED]` is not token-shaped, so a later stage cannot match it
again.

## `scrub_item` per `StoreItem` kind

`scrub_item_with(item, policy)` runs `sanitize_text_with` over each free text
an item carries and merges the reports:

| Kind | Scrubbed | Left alone |
| --- | --- | --- |
| `Document` | `title`, a `DocumentBody::Text` body | a `DocumentBody::Uri` body |
| `Conversation` | every turn's `text` | turn roles |
| `Learning` | `text`, `evidence` | the learning kind |
| any (metadata) | `meta.url` (query strings carry tokens) | paths, repository, commit, thread and agent ids |

Identifiers in the metadata stay as they are because filters match on them, and
rewriting them would make an item impossible to find. A `Uri` body is left
alone because sources resolve it to text before storing, and that text is
scrubbed at that point. `scrub_item` is `scrub_item_with` under the default,
strictest `Policy`.

## Known limits

- **Pattern-based only.** Contextual PII ("call me at the usual number"),
  combinations (name + employer + city), personal names and free-form dates of
  birth need NER or an LLM and are not handled here.
- **Email addresses are detected, not redacted.** `has_likely_email` reports
  them; the content scrubber leaves them in place.
- **Bare 10-digit NANP numbers are not redacted.** A NANP phone needs
  separators or a leading `1` country code to reach its regex.
- **Unknown credential formats get through.** A token without a known prefix,
  a key-like name, or a marker in front of it is not recognised. Under
  `sanitize_json` a sensitive *key* name still catches it.
- **False positives are accepted.** Shape rules can rewrite harmless text, such
  as a 13-digit timestamp under `LuhnOnly`, or a JSON field whose name ends in
  `key`. A redaction inside structured content can corrupt it for whoever wrote
  it; `Policy::corroborated()` limits that for card-shaped numbers.
- **Not reversible.** Redacted values are dropped, not stored somewhere else.
  The report gives counts, never the original values.

## Testing

Unit tests sit beside each module (`mod_tests.rs`, plus
`sanitize/mod_default_policy_tests.rs` and `pii/mod_prefilter_tests.rs`). They
force every `LazyLock` pattern, so a typo in a built-in regex fails CI rather
than a host. Credential fixtures are assembled at run time so repository secret
scanners don't flag them.

[`scrub_item`]: item/mod.rs

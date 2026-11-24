# Office batch core

Bounded, pure in-memory transformations and inspections of PPTX, DOCX and XLSX
packages, on top of shared OOXML package checks. Every call takes input bytes and
returns a new package or a bounded projection without modifying the input. The
typed actions reuse the existing `desk-agent-protocol` live-patch types; the crate
does not define a second command protocol.

- DOCX: `docx_copy` replaces the main body text (`DocumentLivePatchAction`) and
  reads the result back; `docx_inspect` returns bounded stored body text without
  evaluating fields.
- XLSX: `xlsx_edit` prepares a typed cell edit whose package still needs
  recalculation and independent readback before publication; `xlsx_formula_inventory`
  inspects cells and rule formulas across all sheets; `xlsx_native_preflight`
  admits package contents before a host opens a private copy in Excel, and
  `xlsx_native_merge` imports only verified native formula caches.

For PPTX, `pptx_copy` uses `PresentationLivePatchAction`. A call replaces either
the selected slide title or its presenter notes. The title operation preserves run
properties within one paragraph.
`pptx_inspect::inspect` reads the same selected parts into a bounded title/notes
projection, distinguishes missing placeholders from empty text, and reports format
editability separately from authorization. Text limits reject oversized projections
instead of silently truncating the content used for a later approval.
Plain-text notes replacement rebuilds paragraphs, including empty lines, while
retaining the notes shape and body properties. Missing notes parts, ambiguous
placeholders and unsupported title structures are rejected. Package checks are
bounded but are not a complete OOXML validator or a native Office trust decision.

The caller must separately provide owner-authorized immutable input, a fresh
observed target, exact-input approval, supported-format readiness, destination
containment, create-new durable publication and outcome recovery. This crate
does not open files, launch Office, publish artifacts or enable a capability.
Windows host integration remains separate from these format operations.

Validation from the web repository root:

```text
cargo test -p desk-office-batch --offline
cargo clippy -p desk-office-batch --offline --all-targets --no-deps -- -D warnings
```

The parent workspace's `pocs/poc-windows-office-batch` consumes this crate for its
independent python-pptx fixture/readback checks. Production never imports the PoC.

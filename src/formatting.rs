// High level formatting functions.

use std::collections::HashMap;
use std::io::{self, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::time::{Duration, Instant};

use syntax::ast;
use syntax::errors::emitter::{ColorConfig, Emitter};
use syntax::errors::{DiagnosticBuilder, Handler};
use syntax::parse::{self, ParseSess};
use syntax::source_map::{FilePathMapping, SourceMap, Span, DUMMY_SP};

use self::newline_style::apply_newline_style;
use crate::comment::{CharClasses, FullCodeCharKind};
use crate::config::{Config, FileName, Verbosity};
use crate::ignore_path::IgnorePathSet;
use crate::issues::BadIssueSeeker;
use crate::utils::{count_newlines, get_skip_macro_names};
use crate::visitor::{FmtVisitor, SnippetProvider};
use crate::{modules, source_file, ErrorKind, FormatReport, Input, Session};

mod newline_style;

// A map of the files of a crate, with their new content
pub(crate) type SourceFile = Vec<FileRecord>;
pub(crate) type FileRecord = (FileName, String);

impl<'b, T: Write + 'b> Session<'b, T> {
    pub(crate) fn format_input_inner(&mut self, input: Input) -> Result<FormatReport, ErrorKind> {
        if !self.config.version_meets_requirement() {
            return Err(ErrorKind::VersionMismatch);
        }

        syntax::with_globals(self.config.edition().to_libsyntax_pos_edition(), || {
            if self.config.disable_all_formatting() {
                // When the input is from stdin, echo back the input.
                if let Input::Text(ref buf) = input {
                    if let Err(e) = io::stdout().write_all(buf.as_bytes()) {
                        return Err(From::from(e));
                    }
                }
                return Ok(FormatReport::new());
            }

            let config = &self.config.clone();
            let format_result = format_project(input, config, self);

            format_result.map(|report| {
                self.errors.add(&report.internal.borrow().1);
                report
            })
        })
    }
}

// Format an entire crate (or subset of the module tree).
fn format_project<T: FormatHandler>(
    input: Input,
    config: &Config,
    handler: &mut T,
) -> Result<FormatReport, ErrorKind> {
    let mut timer = Timer::start();

    let main_file = input.file_name();
    let input_is_stdin = main_file == FileName::Stdin;

    let ignore_path_set = match IgnorePathSet::from_ignore_list(&config.ignore()) {
        Ok(set) => set,
        Err(e) => return Err(ErrorKind::InvalidGlobPattern(e)),
    };
    if config.skip_children() && ignore_path_set.is_match(&main_file) {
        return Ok(FormatReport::new());
    }

    // Parse the crate.
    let source_map = Rc::new(SourceMap::new(FilePathMapping::empty()));
    let mut parse_session = make_parse_sess(source_map.clone(), config);
    let mut report = FormatReport::new();
    let directory_ownership = input.to_directory_ownership();
    let krate = match parse_crate(
        input,
        &parse_session,
        config,
        &mut report,
        directory_ownership,
    ) {
        Ok(krate) => krate,
        // Surface parse error via Session (errors are merged there from report)
        Err(ErrorKind::ParseError) => return Ok(report),
        Err(e) => return Err(e),
    };
    timer = timer.done_parsing();

    // Suppress error output if we have to do any further parsing.
    let silent_emitter = silent_emitter();
    parse_session.span_diagnostic = Handler::with_emitter(true, None, silent_emitter);

    let mut context = FormatContext::new(&krate, report, parse_session, config, handler);
    let files = modules::ModResolver::new(
        &context.parse_session,
        directory_ownership.unwrap_or(parse::DirectoryOwnership::UnownedViaMod(true)),
        !(input_is_stdin || config.skip_children()),
    )
    .visit_crate(&krate)
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    for (path, (module, _)) in files {
        let should_ignore = !input_is_stdin && ignore_path_set.is_match(&path);
        if (config.skip_children() && path != main_file) || should_ignore {
            continue;
        }
        should_emit_verbose(input_is_stdin, config, || println!("Formatting {}", path));
        let is_root = path == main_file;
        context.format_file(path, &module, is_root)?;
    }
    timer = timer.done_formatting();

    should_emit_verbose(input_is_stdin, config, || {
        println!(
            "Spent {0:.3} secs in the parsing phase, and {1:.3} secs in the formatting phase",
            timer.get_parse_time(),
            timer.get_format_time(),
        )
    });

    Ok(context.report)
}

// Used for formatting files.
#[derive(new)]
struct FormatContext<'a, T: FormatHandler> {
    krate: &'a ast::Crate,
    report: FormatReport,
    parse_session: ParseSess,
    config: &'a Config,
    handler: &'a mut T,
}

impl<'a, T: FormatHandler + 'a> FormatContext<'a, T> {
    // Formats a single file/module.
    fn format_file(
        &mut self,
        path: FileName,
        module: &ast::Mod,
        is_root: bool,
    ) -> Result<(), ErrorKind> {
        let source_file = self
            .parse_session
            .source_map()
            .lookup_char_pos(module.inner.lo())
            .file;
        let big_snippet = source_file.src.as_ref().unwrap();
        let snippet_provider = SnippetProvider::new(source_file.start_pos, big_snippet);
        let mut visitor = FmtVisitor::from_source_map(
            &self.parse_session,
            &self.config,
            &snippet_provider,
            self.report.clone(),
        );
        visitor
            .skip_macro_names
            .borrow_mut()
            .append(&mut get_skip_macro_names(&self.krate.attrs));

        // Format inner attributes if available.
        if !self.krate.attrs.is_empty() && is_root {
            visitor.skip_empty_lines(source_file.end_pos);
            if visitor.visit_attrs(&self.krate.attrs, ast::AttrStyle::Inner) {
                visitor.push_rewrite(module.inner, None);
            } else {
                visitor.format_separate_mod(module, &*source_file);
            }
        } else {
            visitor.last_pos = source_file.start_pos;
            visitor.skip_empty_lines(source_file.end_pos);
            visitor.format_separate_mod(module, &*source_file);
        };

        debug_assert_eq!(visitor.line_number, count_newlines(&visitor.buffer));

        // For some reason, the source_map does not include terminating
        // newlines so we must add one on for each file. This is sad.
        source_file::append_newline(&mut visitor.buffer);

        format_lines(
            &mut visitor.buffer,
            &path,
            &visitor.skipped_range,
            &self.config,
            &self.report,
        );

        apply_newline_style(
            self.config.newline_style(),
            &mut visitor.buffer,
            &big_snippet,
        );

        if visitor.macro_rewrite_failure {
            self.report.add_macro_format_failure();
        }
        self.report
            .add_non_formatted_ranges(visitor.skipped_range.clone());

        self.handler.handle_formatted_file(
            self.parse_session.source_map(),
            path,
            visitor.buffer.to_owned(),
            &mut self.report,
        )
    }
}

// Handle the results of formatting.
trait FormatHandler {
    fn handle_formatted_file(
        &mut self,
        source_map: &SourceMap,
        path: FileName,
        result: String,
        report: &mut FormatReport,
    ) -> Result<(), ErrorKind>;
}

impl<'b, T: Write + 'b> FormatHandler for Session<'b, T> {
    // Called for each formatted file.
    fn handle_formatted_file(
        &mut self,
        source_map: &SourceMap,
        path: FileName,
        result: String,
        report: &mut FormatReport,
    ) -> Result<(), ErrorKind> {
        if let Some(ref mut out) = self.out {
            match source_file::write_file(Some(source_map), &path, &result, out, &self.config) {
                Ok(has_diff) if has_diff => report.add_diff(),
                Err(e) => {
                    // Create a new error with path_str to help users see which files failed
                    let err_msg = format!("{}: {}", path, e);
                    return Err(io::Error::new(e.kind(), err_msg).into());
                }
                _ => {}
            }
        }

        self.source_file.push((path, result));
        Ok(())
    }
}

pub(crate) struct FormattingError {
    pub(crate) line: usize,
    pub(crate) kind: ErrorKind,
    is_comment: bool,
    is_string: bool,
    pub(crate) line_buffer: String,
}

impl FormattingError {
    pub(crate) fn from_span(
        span: Span,
        source_map: &SourceMap,
        kind: ErrorKind,
    ) -> FormattingError {
        FormattingError {
            line: source_map.lookup_char_pos(span.lo()).line,
            is_comment: kind.is_comment(),
            kind,
            is_string: false,
            line_buffer: source_map
                .span_to_lines(span)
                .ok()
                .and_then(|fl| {
                    fl.file
                        .get_line(fl.lines[0].line_index)
                        .map(std::borrow::Cow::into_owned)
                })
                .unwrap_or_else(String::new),
        }
    }

    pub(crate) fn is_internal(&self) -> bool {
        match self.kind {
            ErrorKind::LineOverflow(..)
            | ErrorKind::TrailingWhitespace
            | ErrorKind::IoError(_)
            | ErrorKind::ParseError
            | ErrorKind::LostComment => true,
            _ => false,
        }
    }

    pub(crate) fn msg_suffix(&self) -> &str {
        if self.is_comment || self.is_string {
            "set `error_on_unformatted = false` to suppress \
             the warning against comments or string literals\n"
        } else {
            ""
        }
    }

    // (space, target)
    pub(crate) fn format_len(&self) -> (usize, usize) {
        match self.kind {
            ErrorKind::LineOverflow(found, max) => (max, found - max),
            ErrorKind::TrailingWhitespace
            | ErrorKind::DeprecatedAttr
            | ErrorKind::BadIssue(_)
            | ErrorKind::BadAttr
            | ErrorKind::LostComment => {
                let trailing_ws_start = self
                    .line_buffer
                    .rfind(|c: char| !c.is_whitespace())
                    .map(|pos| pos + 1)
                    .unwrap_or(0);
                (
                    trailing_ws_start,
                    self.line_buffer.len() - trailing_ws_start,
                )
            }
            _ => unreachable!(),
        }
    }
}

pub(crate) type FormatErrorMap = HashMap<FileName, Vec<FormattingError>>;

#[derive(Default, Debug, PartialEq)]
pub(crate) struct ReportedErrors {
    // Encountered e.g., an IO error.
    pub(crate) has_operational_errors: bool,

    // Failed to reformat code because of parsing errors.
    pub(crate) has_parsing_errors: bool,

    // Code is valid, but it is impossible to format it properly.
    pub(crate) has_formatting_errors: bool,

    // Code contains macro call that was unable to format.
    pub(crate) has_macro_format_failure: bool,

    // Failed a check, such as the license check or other opt-in checking.
    pub(crate) has_check_errors: bool,

    /// Formatted code differs from existing code (--check only).
    pub(crate) has_diff: bool,
}

impl ReportedErrors {
    /// Combine two summaries together.
    pub(crate) fn add(&mut self, other: &ReportedErrors) {
        self.has_operational_errors |= other.has_operational_errors;
        self.has_parsing_errors |= other.has_parsing_errors;
        self.has_formatting_errors |= other.has_formatting_errors;
        self.has_macro_format_failure |= other.has_macro_format_failure;
        self.has_check_errors |= other.has_check_errors;
        self.has_diff |= other.has_diff;
    }
}

#[derive(Clone, Copy, Debug)]
enum Timer {
    Disabled,
    Initialized(Instant),
    DoneParsing(Instant, Instant),
    DoneFormatting(Instant, Instant, Instant),
}

impl Timer {
    fn start() -> Timer {
        if cfg!(target_arch = "wasm32") {
            Timer::Disabled
        } else {
            Timer::Initialized(Instant::now())
        }
    }
    fn done_parsing(self) -> Self {
        match self {
            Timer::Disabled => Timer::Disabled,
            Timer::Initialized(init_time) => Timer::DoneParsing(init_time, Instant::now()),
            _ => panic!("Timer can only transition to DoneParsing from Initialized state"),
        }
    }

    fn done_formatting(self) -> Self {
        match self {
            Timer::Disabled => Timer::Disabled,
            Timer::DoneParsing(init_time, parse_time) => {
                Timer::DoneFormatting(init_time, parse_time, Instant::now())
            }
            _ => panic!("Timer can only transition to DoneFormatting from DoneParsing state"),
        }
    }

    /// Returns the time it took to parse the source files in seconds.
    fn get_parse_time(&self) -> f32 {
        match *self {
            Timer::Disabled => panic!("this platform cannot time execution"),
            Timer::DoneParsing(init, parse_time) | Timer::DoneFormatting(init, parse_time, _) => {
                // This should never underflow since `Instant::now()` guarantees monotonicity.
                Self::duration_to_f32(parse_time.duration_since(init))
            }
            Timer::Initialized(..) => unreachable!(),
        }
    }

    /// Returns the time it took to go from the parsed AST to the formatted output. Parsing time is
    /// not included.
    fn get_format_time(&self) -> f32 {
        match *self {
            Timer::Disabled => panic!("this platform cannot time execution"),
            Timer::DoneFormatting(_init, parse_time, format_time) => {
                Self::duration_to_f32(format_time.duration_since(parse_time))
            }
            Timer::DoneParsing(..) | Timer::Initialized(..) => unreachable!(),
        }
    }

    fn duration_to_f32(d: Duration) -> f32 {
        d.as_secs() as f32 + d.subsec_nanos() as f32 / 1_000_000_000f32
    }
}

// Formatting done on a char by char or line by line basis.
// FIXME(#20): other stuff for parity with make tidy.
fn format_lines(
    text: &mut String,
    name: &FileName,
    skipped_range: &[(usize, usize)],
    config: &Config,
    report: &FormatReport,
) {
    let mut formatter = FormatLines::new(name, skipped_range, config);
    formatter.check_license(text);
    formatter.iterate(text);

    if formatter.newline_count > 1 {
        debug!("track truncate: {} {}", text.len(), formatter.newline_count);
        let line = text.len() - formatter.newline_count + 1;
        text.truncate(line);
    }

    report.append(name.clone(), formatter.errors);
}

struct FormatLines<'a> {
    name: &'a FileName,
    skipped_range: &'a [(usize, usize)],
    last_was_space: bool,
    line_len: usize,
    cur_line: usize,
    newline_count: usize,
    errors: Vec<FormattingError>,
    issue_seeker: BadIssueSeeker,
    line_buffer: String,
    current_line_contains_string_literal: bool,
    format_line: bool,
    allow_issue_seek: bool,
    config: &'a Config,
}

impl<'a> FormatLines<'a> {
    fn new(
        name: &'a FileName,
        skipped_range: &'a [(usize, usize)],
        config: &'a Config,
    ) -> FormatLines<'a> {
        let issue_seeker = BadIssueSeeker::new(config.report_todo(), config.report_fixme());
        FormatLines {
            name,
            skipped_range,
            last_was_space: false,
            line_len: 0,
            cur_line: 1,
            newline_count: 0,
            errors: vec![],
            allow_issue_seek: !issue_seeker.is_disabled(),
            issue_seeker,
            line_buffer: String::with_capacity(config.max_width() * 2),
            current_line_contains_string_literal: false,
            format_line: config.file_lines().contains_line(name, 1),
            config,
        }
    }

    fn check_license(&mut self, text: &mut String) {
        if let Some(ref license_template) = self.config.license_template {
            if !license_template.is_match(text) {
                self.errors.push(FormattingError {
                    line: self.cur_line,
                    kind: ErrorKind::LicenseCheck,
                    is_comment: false,
                    is_string: false,
                    line_buffer: String::new(),
                });
            }
        }
    }

    // Iterate over the chars in the file map.
    fn iterate(&mut self, text: &mut String) {
        for (kind, c) in CharClasses::new(text.chars()) {
            if c == '\r' {
                continue;
            }

            if self.allow_issue_seek && self.format_line {
                // Add warnings for bad todos/ fixmes
                if let Some(issue) = self.issue_seeker.inspect(c) {
                    self.push_err(ErrorKind::BadIssue(issue), false, false);
                }
            }

            if c == '\n' {
                self.new_line(kind);
            } else {
                self.char(c, kind);
            }
        }
    }

    fn new_line(&mut self, kind: FullCodeCharKind) {
        if self.format_line {
            // Check for (and record) trailing whitespace.
            if self.last_was_space {
                if self.should_report_error(kind, &ErrorKind::TrailingWhitespace)
                    && !self.is_skipped_line()
                {
                    self.push_err(
                        ErrorKind::TrailingWhitespace,
                        kind.is_comment(),
                        kind.is_string(),
                    );
                }
                self.line_len -= 1;
            }

            // Check for any line width errors we couldn't correct.
            let error_kind = ErrorKind::LineOverflow(self.line_len, self.config.max_width());
            if self.line_len > self.config.max_width()
                && !self.is_skipped_line()
                && self.should_report_error(kind, &error_kind)
            {
                let is_string = self.current_line_contains_string_literal;
                self.push_err(error_kind, kind.is_comment(), is_string);
            }
        }

        self.line_len = 0;
        self.cur_line += 1;
        self.format_line = self
            .config
            .file_lines()
            .contains_line(self.name, self.cur_line);
        self.newline_count += 1;
        self.last_was_space = false;
        self.line_buffer.clear();
        self.current_line_contains_string_literal = false;
    }

    fn char(&mut self, c: char, kind: FullCodeCharKind) {
        self.newline_count = 0;
        self.line_len += if c == '\t' {
            self.config.tab_spaces()
        } else {
            1
        };
        self.last_was_space = c.is_whitespace();
        self.line_buffer.push(c);
        if kind.is_string() {
            self.current_line_contains_string_literal = true;
        }
    }

    fn push_err(&mut self, kind: ErrorKind, is_comment: bool, is_string: bool) {
        self.errors.push(FormattingError {
            line: self.cur_line,
            kind,
            is_comment,
            is_string,
            line_buffer: self.line_buffer.clone(),
        });
    }

    fn should_report_error(&self, char_kind: FullCodeCharKind, error_kind: &ErrorKind) -> bool {
        let allow_error_report = if char_kind.is_comment()
            || self.current_line_contains_string_literal
            || error_kind.is_comment()
        {
            self.config.error_on_unformatted()
        } else {
            true
        };

        match error_kind {
            ErrorKind::LineOverflow(..) => {
                self.config.error_on_line_overflow() && allow_error_report
            }
            ErrorKind::TrailingWhitespace | ErrorKind::LostComment => allow_error_report,
            _ => true,
        }
    }

    /// Returns `true` if the line with the given line number was skipped by `#[rustfmt::skip]`.
    fn is_skipped_line(&self) -> bool {
        self.skipped_range
            .iter()
            .any(|&(lo, hi)| lo <= self.cur_line && self.cur_line <= hi)
    }
}

fn parse_crate(
    input: Input,
    parse_session: &ParseSess,
    config: &Config,
    report: &mut FormatReport,
    directory_ownership: Option<parse::DirectoryOwnership>,
) -> Result<ast::Crate, ErrorKind> {
    let input_is_stdin = input.is_text();

    let parser = match input {
        Input::File(ref file) => {
            // Use `new_sub_parser_from_file` when we the input is a submodule.
            Ok(if let Some(dir_own) = directory_ownership {
                parse::new_sub_parser_from_file(parse_session, file, dir_own, None, DUMMY_SP)
            } else {
                parse::new_parser_from_file(parse_session, file)
            })
        }
        Input::Text(text) => parse::maybe_new_parser_from_source_str(
            parse_session,
            syntax::source_map::FileName::Custom("stdin".to_owned()),
            text,
        )
        .map(|mut parser| {
            parser.recurse_into_file_modules = false;
            parser
        })
        .map_err(|diags| {
            diags
                .into_iter()
                .map(|d| DiagnosticBuilder::new_diagnostic(&parse_session.span_diagnostic, d))
                .collect::<Vec<_>>()
        }),
    };

    let result = match parser {
        Ok(mut parser) => {
            parser.cfg_mods = false;
            if config.skip_children() {
                parser.recurse_into_file_modules = false;
            }

            let mut parser = AssertUnwindSafe(parser);
            catch_unwind(move || parser.0.parse_crate_mod().map_err(|d| vec![d]))
        }
        Err(db) => Ok(Err(db)),
    };

    match result {
        Ok(Ok(c)) => {
            if !parse_session.span_diagnostic.has_errors() {
                return Ok(c);
            }
        }
        Ok(Err(mut diagnostics)) => diagnostics.iter_mut().for_each(DiagnosticBuilder::emit),
        Err(_) => {
            // Note that if you see this message and want more information,
            // then run the `parse_crate_mod` function above without
            // `catch_unwind` so rustfmt panics and you can get a backtrace.
            should_emit_verbose(input_is_stdin, config, || {
                println!("The Rust parser panicked")
            });
        }
    }

    report.add_parsing_error();
    Err(ErrorKind::ParseError)
}

/// Emitter which discards every error.
struct SilentEmitter;

impl Emitter for SilentEmitter {
    fn emit_diagnostic(&mut self, _db: &DiagnosticBuilder<'_>) {}
}

fn silent_emitter() -> Box<SilentEmitter> {
    Box::new(SilentEmitter {})
}

fn make_parse_sess(source_map: Rc<SourceMap>, config: &Config) -> ParseSess {
    let tty_handler = if config.hide_parse_errors() {
        let silent_emitter = silent_emitter();
        Handler::with_emitter(true, None, silent_emitter)
    } else {
        let supports_color = term::stderr().map_or(false, |term| term.supports_color());
        let color_cfg = if supports_color {
            ColorConfig::Auto
        } else {
            ColorConfig::Never
        };
        Handler::with_tty_emitter(color_cfg, true, None, Some(source_map.clone()))
    };

    ParseSess::with_span_handler(tty_handler, source_map)
}

fn should_emit_verbose<F>(is_stdin: bool, config: &Config, f: F)
where
    F: Fn(),
{
    if config.verbose() == Verbosity::Verbose && !is_stdin {
        f();
    }
}

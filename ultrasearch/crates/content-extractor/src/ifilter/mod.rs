//! Machine-registered IFilters over verified snapshots, never mutable source paths.
#![cfg(any(windows, test))]

use crate::{ExtractContext, ExtractError, ExtractedContent};
use core_types::DocKey;

#[cfg(windows)]
use crate::Extractor;
#[cfg(windows)]
use std::{fs, io::Seek};

#[cfg(windows)]
pub struct IFilterExtractor;

#[cfg(windows)]
impl IFilterExtractor {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(windows)]
impl Default for IFilterExtractor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl Extractor for IFilterExtractor {
    fn name(&self) -> &'static str {
        "ifilter"
    }

    fn supports(&self, ctx: &ExtractContext) -> bool {
        super::resolve_ext(ctx).is_some_and(|ext| supported_extension(&ext))
    }

    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        let file = fs::File::open(ctx.path).map_err(|error| failed("open input", error))?;
        self.extract_file(ctx, key, &file)
    }

    fn extract_file(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        file: &fs::File,
    ) -> Result<ExtractedContent, ExtractError> {
        let size = file
            .metadata()
            .map_err(|error| failed("input metadata", error))?
            .len();
        let max_bytes = ctx.max_bytes.min(u32::MAX as usize);
        if size > max_bytes as u64 {
            return Err(ExtractError::FileTooLarge {
                bytes: size,
                max_bytes: max_bytes as u64,
            });
        }
        let mut source = file;
        source
            .rewind()
            .map_err(|error| failed("rewind input", error))?;
        let bytes = super::read_bounded(source, max_bytes)?;
        self.extract_bytes(ctx, key, &bytes)
    }

    fn extract_bytes(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        bytes: &[u8],
    ) -> Result<ExtractedContent, ExtractError> {
        if bytes.len() > ctx.max_bytes || bytes.len() > u32::MAX as usize {
            return Err(ExtractError::FileTooLarge {
                bytes: bytes.len() as u64,
                max_bytes: (ctx.max_bytes as u64).min(u64::from(u32::MAX)),
            });
        }
        let ext = super::resolve_ext(ctx).unwrap_or_default();
        if !supported_extension(&ext) {
            return Err(ExtractError::Unsupported(format!("IFilter extension {ext:?}")));
        }
        if bytes.is_empty() {
            return TextOutput::new(ctx).finish(key);
        }
        native::extract(&ext, bytes, ctx, key)
    }
}

fn supported_extension(ext: &str) -> bool {
    matches!(ext, "rtf" | "odt" | "msg" | "doc" | "xls" | "ppt")
}

fn failed(operation: &str, error: impl std::fmt::Display) -> ExtractError {
    ExtractError::Failed(format!("IFilter {operation}: {error}"))
}

/// Validate GUIDs before interpolating machine registration values into keys.
/// Never ask CLSIDFromString to resolve arbitrary ProgIDs (it may register one).
fn guid_number(value: &str) -> Result<u128, ExtractError> {
    let inner = value
        .strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'));
    let Some(inner) = inner.filter(|v| v.len() == 36) else {
        return Err(failed("registration", "expected a braced GUID"));
    };
    if !inner.bytes().enumerate().all(|(i, b)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            b == b'-'
        } else {
            b.is_ascii_hexdigit()
        }
    }) {
        return Err(failed("registration", "invalid GUID"));
    }
    u128::from_str_radix(&inner.replace('-', ""), 16)
        .map_err(|error| failed("registration GUID", error))
}

/// Follow the documented extension -> persistent handler -> IFilter mapping.
/// Lookup reads machine Software\Classes only; no source file is opened and
/// no generic text-filter fallback can turn an unknown binary into text.
/// https://learn.microsoft.com/windows/win32/search/-search-ifilter-registering-filters
fn registered_filter(
    ext: &str,
    mut lookup: impl FnMut(&str) -> Result<Option<String>, ExtractError>,
) -> Result<u128, ExtractError> {
    if !supported_extension(ext) {
        return Err(ExtractError::Unsupported(format!("IFilter extension {ext:?}")));
    }
    let missing = || ExtractError::Unsupported(format!("no machine-registered IFilter for .{ext}"));
    let persistent = match lookup(&format!(".{ext}\\PersistentHandler"))? {
        Some(value) => value,
        None => {
            // Legacy registrations associate the extension with a document
            // ProgID, whose CLSID in turn names its persistent handler.
            let progid = lookup(&format!(".{ext}"))?.ok_or_else(missing)?;
            if progid.is_empty()
                || !progid.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
                })
            {
                return Err(failed("registration", "invalid document ProgID"));
            }
            let application = lookup(&format!("{progid}\\CLSID"))?.ok_or_else(missing)?;
            guid_number(&application)?;
            lookup(&format!("CLSID\\{application}\\PersistentHandler"))?
                .ok_or_else(missing)?
        }
    };
    guid_number(&persistent)?;
    let filter = lookup(&format!(
        "CLSID\\{persistent}\\PersistentAddinsRegistered\\{{89BCB740-6119-101A-BCB7-00DD010655AF}}"
    ))?
    .ok_or_else(missing)?;
    guid_number(&filter)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boundary {
    None,
    Space,
    Paragraph,
}

struct Chunk {
    is_text: bool,
    boundary: Boundary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextEnd {
    More,
    Last,
    End,
}

trait FilterSource {
    fn next_chunk(&mut self) -> Result<Option<Chunk>, ExtractError>;
    fn get_text(&mut self, buffer: &mut [u16]) -> Result<(usize, TextEnd), ExtractError>;
}

/// One document-wide budget and one pending UTF-16 high surrogate. GetText can
/// split a word or surrogate pair across calls; never decode each buffer lossy.
struct TextOutput {
    text: String,
    chars: usize,
    max_bytes: usize,
    max_chars: usize,
    high: Option<u16>,
    separator: Option<char>,
    truncated: bool,
}

impl TextOutput {
    fn new(ctx: &ExtractContext) -> Self {
        Self {
            text: String::new(),
            chars: 0,
            max_bytes: ctx.max_bytes,
            max_chars: ctx.max_chars,
            high: None,
            separator: None,
            truncated: false,
        }
    }

    fn begin_chunk(&mut self, boundary: Boundary) -> Result<(), ExtractError> {
        if boundary != Boundary::None && self.high.is_some() {
            return Err(failed("text", "surrogate pair crosses a chunk break"));
        }
        if !self.text.is_empty() {
            match boundary {
                Boundary::Space if self.separator.is_none() => self.separator = Some(' '),
                Boundary::Paragraph => self.separator = Some('\n'),
                Boundary::None | Boundary::Space => {}
            }
        }
        Ok(())
    }

    fn push_char(&mut self, ch: char) -> bool {
        if self.chars >= self.max_chars
            || ch.len_utf8() > self.max_bytes.saturating_sub(self.text.len())
        {
            self.truncated = true;
            return false;
        }
        self.text.push(ch);
        self.chars += 1;
        true
    }

    fn push(&mut self, units: &[u16]) -> Result<bool, ExtractError> {
        // Empty chunks must not add separators or consume the output budget.
        if !units.is_empty()
            && let Some(separator) = self.separator.take()
            && !self.push_char(separator)
        {
            return Ok(false);
        }
        for &unit in units {
            let scalar = if let Some(high) = self.high.take() {
                if !(0xdc00..=0xdfff).contains(&unit) {
                    return Err(failed("text", "unpaired UTF-16 high surrogate"));
                }
                0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00)
            } else if (0xd800..=0xdbff).contains(&unit) {
                self.high = Some(unit);
                continue;
            } else {
                u32::from(unit)
            };
            let ch = char::from_u32(scalar)
                .ok_or_else(|| failed("text", "invalid UTF-16 scalar"))?;
            if !self.push_char(ch) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn finish(self, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        if self.high.is_some() && !self.truncated {
            return Err(failed("text", "incomplete UTF-16 at end of document"));
        }
        Ok(ExtractedContent {
            key,
            bytes_processed: self.text.len(),
            text: self.text,
            lang: None,
            content_lang: None,
            truncated: self.truncated,
        })
    }
}

/// The same bounded state machine serves native calls and deterministic tests.
/// Unexpected failures invalidate the whole result, not just its final chunk.
fn collect_text(
    source: &mut impl FilterSource,
    ctx: &ExtractContext,
    key: DocKey,
    mut check_budget: impl FnMut() -> Result<(), ExtractError>,
) -> Result<ExtractedContent, ExtractError> {
    let mut output = TextOutput::new(ctx);
    let mut buffer = [0u16; 4096];
    loop {
        check_budget()?;
        let chunk = source.next_chunk()?;
        check_budget()?;
        let Some(chunk) = chunk else {
            return output.finish(key);
        };
        if !chunk.is_text {
            continue;
        }
        output.begin_chunk(chunk.boundary)?;
        loop {
            check_budget()?;
            let (length, end) = source.get_text(&mut buffer)?;
            check_budget()?;
            if length > buffer.len() || (end == TextEnd::End && length != 0) {
                return Err(failed("GetText", "invalid returned buffer length"));
            }
            if end == TextEnd::More && length == 0 {
                return Err(failed("GetText", "success without text or progress"));
            }
            // FILTER_S_LAST_TEXT contains real text; consume it before ending
            // the chunk. Only FILTER_E_NO_MORE_TEXT has no returned data.
            if !output.push(&buffer[..length])? {
                return output.finish(key);
            }
            if end != TextEnd::More {
                break;
            }
        }
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, E_NOINTERFACE, REGDB_E_CLASSNOTREG,
        RPC_E_CHANGED_MODE, S_OK,
    };
    use windows::Win32::Storage::IndexServer::{
        CHUNK_EOC, CHUNK_EOP, CHUNK_EOS, CHUNK_EOW, CHUNK_NO_BREAK, CHUNK_TEXT,
        FILTER_E_END_OF_CHUNKS, FILTER_E_NO_MORE_TEXT, FILTER_S_LAST_TEXT, IFilter,
        IFILTER_INIT_APPLY_INDEX_ATTRIBUTES, IFILTER_INIT_CANON_PARAGRAPHS,
        IFILTER_INIT_CANON_SPACES, IFILTER_INIT_INDEXING_ONLY, STAT_CHUNK,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IPersistStream,
    };
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::Win32::UI::Shell::SHCreateMemStream;
    use windows::core::{ComInterface, GUID, HRESULT, PCWSTR, PWSTR};

    struct ComGuard(bool);
    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: balance only our successful initialization, on the
                // same thread, after every filter/stream interface has dropped.
                unsafe {
                    CoUninitialize();
                }
            }
        }
    }

    fn registration_value(key: &str) -> Result<Option<String>, ExtractError> {
        let key: Vec<u16> = format!("Software\\Classes\\{key}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut data = [0u16; 512];
        let mut bytes = std::mem::size_of_val(&data) as u32;
        // SAFETY: predefined machine root, terminated key, bounded aligned
        // buffer and live byte count. This is a read-only REG_SZ lookup.
        let result = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key.as_ptr()),
                PCWSTR::null(),
                RRF_RT_REG_SZ,
                None,
                Some(data.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if let Err(error) = result {
            if [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
                .iter()
                .any(|code| error.code() == HRESULT::from_win32(code.0))
            {
                return Ok(None);
            }
            return Err(failed("registry read", error));
        }
        if bytes < 2
            || bytes as usize > std::mem::size_of_val(&data)
            || !bytes.is_multiple_of(2)
        {
            return Err(failed("registry read", "invalid REG_SZ length"));
        }
        let value = &data[..bytes as usize / 2];
        if value.last() != Some(&0) || value[..value.len() - 1].contains(&0) {
            return Err(failed("registry read", "invalid REG_SZ termination"));
        }
        String::from_utf16(&value[..value.len() - 1])
            .map(Some)
            .map_err(|error| failed("registry Unicode", error))
    }

    struct NativeFilter {
        filter: IFilter,
        stat: STAT_CHUNK,
    }

    impl FilterSource for NativeFilter {
        fn next_chunk(&mut self) -> Result<Option<Chunk>, ExtractError> {
            // SAFETY: filter and descriptor stay alive. Retain the descriptor:
            // CHUNK_NO_BREAK implementations may update only its idChunk.
            let hr = unsafe { self.filter.GetChunk(&mut self.stat) };
            if hr == FILTER_E_END_OF_CHUNKS.0 {
                return Ok(None);
            }
            if hr != S_OK.0 {
                return Err(failed("GetChunk", format!("HRESULT 0x{hr:08x}")));
            }
            let boundary = if self.stat.breakType == CHUNK_NO_BREAK {
                Boundary::None
            } else if self.stat.breakType == CHUNK_EOW {
                Boundary::Space
            } else if [CHUNK_EOS, CHUNK_EOP, CHUNK_EOC].contains(&self.stat.breakType) {
                Boundary::Paragraph
            } else {
                return Err(failed("GetChunk", "unknown chunk break type"));
            };
            Ok(Some(Chunk {
                is_text: self.stat.flags.0 & CHUNK_TEXT.0 != 0,
                boundary,
            }))
        }

        fn get_text(&mut self, buffer: &mut [u16]) -> Result<(usize, TextEnd), ExtractError> {
            let mut length = buffer.len() as u32;
            // SAFETY: buffer is writable for exactly the supplied capacity.
            let hr = unsafe { self.filter.GetText(&mut length, PWSTR(buffer.as_mut_ptr())) };
            if hr == FILTER_E_NO_MORE_TEXT.0 {
                return Ok((0, TextEnd::End));
            }
            let end = if hr == S_OK.0 {
                TextEnd::More
            } else if hr == FILTER_S_LAST_TEXT.0 {
                TextEnd::Last
            } else {
                return Err(failed("GetText", format!("HRESULT 0x{hr:08x}")));
            };
            Ok((length as usize, end))
        }
    }

    pub(super) fn extract(
        ext: &str,
        bytes: &[u8],
        ctx: &ExtractContext,
        key: DocKey,
    ) -> Result<ExtractedContent, ExtractError> {
        let started = Instant::now();
        let clsid = GUID::from_u128(registered_filter(ext, registration_value)?);
        // SAFETY: initialization and all COM use remain on this thread. Reuse
        // an existing STA without decrementing another caller's COM balance.
        let _com = match unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) } {
            // windows 0.52 projects both S_OK and S_FALSE to Ok(()). Both
            // successful calls acquire a balance that we must release.
            Ok(()) => ComGuard(true),
            Err(error) if error.code() == RPC_E_CHANGED_MODE => ComGuard(false),
            Err(error) => return Err(failed("CoInitializeEx", error)),
        };
        // SAFETY: extract_bytes checked the u32-sized buffer limit. Declare
        // the owned copy before the filter so it outlives every filter interface.
        let stream = unsafe { SHCreateMemStream(Some(bytes)) }
            .ok_or_else(|| failed("memory stream", "allocation failed"))?;
        // SAFETY: activate only the machine-registered class for this extension.
        // No input pathname or user-controlled class identifier is supplied.
        let filter: IFilter = unsafe { CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER) }
            .map_err(|error| {
                if [REGDB_E_CLASSNOTREG, E_NOINTERFACE].contains(&error.code()) {
                    ExtractError::Unsupported(format!("IFilter unavailable for .{ext}: {error}"))
                } else {
                    failed("activation", error)
                }
            })?;
        let persistence: IPersistStream = filter.cast().map_err(|error| {
            if error.code() == E_NOINTERFACE {
                ExtractError::Unsupported(format!("IFilter for .{ext} has no IPersistStream"))
            } else {
                failed("IPersistStream query", error)
            }
        })?;
        // SAFETY: feed only the verified memory stream. Never use IPersistFile
        // or LoadIFilter, including as a fallback for path-only registrations.
        unsafe { persistence.Load(&stream) }.map_err(|error| failed("stream load", error))?;
        let flags = (IFILTER_INIT_CANON_PARAGRAPHS.0
            | IFILTER_INIT_CANON_SPACES.0
            | IFILTER_INIT_APPLY_INDEX_ATTRIBUTES.0
            | IFILTER_INIT_INDEXING_ONLY.0) as u32;
        let mut reported = 0;
        // Do not request IFILTER_INIT_SEARCH_LINKS: a snapshot extraction must
        // not ask a filter to follow linked documents outside its input stream.
        let hr = unsafe { filter.Init(flags, &[], &mut reported) };
        if hr < 0 {
            return Err(failed("Init", format!("HRESULT 0x{hr:08x}")));
        }
        let mut source = NativeFilter {
            filter,
            stat: STAT_CHUNK::default(),
        };
        let mut checks = 0usize;
        collect_text(&mut source, ctx, key, || {
            checks += 1;
            // One pre/post pair per native call, including non-text chunks.
            // This is cooperative; the worker parent still contains hung DLLs.
            if checks > 131_072 || started.elapsed() >= Duration::from_secs(30) {
                return Err(failed(
                    "collection budget",
                    "filter did not complete within its work limit",
                ));
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Fixture {
        chunks: VecDeque<Result<Option<Chunk>, ExtractError>>,
        text: VecDeque<Result<(Vec<u16>, TextEnd), ExtractError>>,
    }

    impl FilterSource for Fixture {
        fn next_chunk(&mut self) -> Result<Option<Chunk>, ExtractError> {
            self.chunks.pop_front().expect("unexpected chunk read")
        }

        fn get_text(&mut self, buffer: &mut [u16]) -> Result<(usize, TextEnd), ExtractError> {
            let (text, end) = self.text.pop_front().expect("unexpected text read")?;
            let copied = text.len().min(buffer.len());
            buffer[..copied].copy_from_slice(&text[..copied]);
            Ok((text.len(), end))
        }
    }

    fn ctx(bytes: usize, chars: usize) -> ExtractContext<'static> {
        ExtractContext {
            path: "missing.rtf",
            max_bytes: bytes,
            max_chars: chars,
            ext_hint: Some("rtf"),
            mime_hint: None,
        }
    }

    fn text_chunk(boundary: Boundary) -> Result<Option<Chunk>, ExtractError> {
        Ok(Some(Chunk {
            is_text: true,
            boundary,
        }))
    }

    fn part(text: &str, end: TextEnd) -> Result<(Vec<u16>, TextEnd), ExtractError> {
        Ok((text.encode_utf16().collect(), end))
    }

    fn key() -> DocKey {
        DocKey::from_parts(42, 0xfedc_0000_0000_0071)
    }

    #[test]
    fn final_text_and_split_unicode_survive_chunk_boundaries() {
        let mut input = Fixture {
            chunks: VecDeque::from([
                text_chunk(Boundary::None),
                text_chunk(Boundary::None),
                text_chunk(Boundary::Paragraph),
                Ok(None),
            ]),
            text: VecDeque::from([
                part("hello ", TextEnd::More),
                Ok((vec![0xd83d], TextEnd::Last)),
                Ok((vec![0xde00], TextEnd::Last)),
                part("world", TextEnd::Last),
            ]),
        };
        let output = collect_text(&mut input, &ctx(100, 100), key(), || Ok(())).unwrap();
        assert_eq!(output.text, "hello 😀\nworld");
        assert_eq!(output.bytes_processed, output.text.len());
        assert_eq!(output.key, key());
        assert!(!output.truncated);
        assert!(input.chunks.is_empty() && input.text.is_empty());
    }

    #[test]
    fn limits_apply_to_the_whole_document_without_splitting_scalars() {
        for (bytes, chars, expected) in [
            (6, 100, "😀é"),
            (100, 2, "😀é"),
            (5, 100, "😀"),
            (0, 0, ""),
        ] {
            let mut input = Fixture {
                chunks: VecDeque::from([
                    text_chunk(Boundary::None),
                    text_chunk(Boundary::None),
                ]),
                text: VecDeque::from([part("😀", TextEnd::Last), part("éx", TextEnd::Last)]),
            };
            let out = collect_text(&mut input, &ctx(bytes, chars), key(), || Ok(())).unwrap();
            assert_eq!(out.text, expected);
            assert_eq!(out.bytes_processed, expected.len());
            assert!(out.truncated);
        }
        // Exactly filling the budget does not by itself establish truncation.
        let mut input = Fixture {
            chunks: VecDeque::from([
                text_chunk(Boundary::None),
                text_chunk(Boundary::Space),
                Ok(None),
            ]),
            text: VecDeque::from([part("é", TextEnd::Last), part("", TextEnd::End)]),
        };
        let out = collect_text(&mut input, &ctx(2, 1), key(), || Ok(())).unwrap();
        assert_eq!(out.text, "é");
        assert!(!out.truncated);
    }

    #[test]
    fn chunk_separators_share_limits_and_empty_chunks_do_not_add_text() {
        for (limit, expected, truncated) in [
            (100, "one two\nthree", false),
            (7, "one two", true),
            (8, "one two\n", true),
        ] {
            let mut input = Fixture {
                chunks: VecDeque::from([
                    text_chunk(Boundary::Paragraph),
                    text_chunk(Boundary::Space),
                    text_chunk(Boundary::Paragraph),
                    text_chunk(Boundary::Space),
                    Ok(None),
                ]),
                text: VecDeque::from([
                    part("one", TextEnd::Last),
                    part("two", TextEnd::Last),
                    part("", TextEnd::End),
                    part("three", TextEnd::Last),
                ]),
            };
            let output = collect_text(&mut input, &ctx(limit, 100), key(), || Ok(())).unwrap();
            assert_eq!(output.text, expected);
            assert_eq!(output.bytes_processed, expected.len());
            assert_eq!(output.truncated, truncated);
        }
    }

    #[test]
    fn errors_and_nonprogress_never_publish_a_partial_success() {
        for failure in [
            Err(failed("GetText", "denied")),
            Ok((vec![], TextEnd::More)),
            Ok((vec![u16::from(b'x'); 4097], TextEnd::Last)),
            Ok((vec![u16::from(b'x')], TextEnd::End)),
        ] {
            let mut input = Fixture {
                chunks: VecDeque::from([text_chunk(Boundary::None)]),
                text: VecDeque::from([part("prefix", TextEnd::More), failure]),
            };
            assert!(matches!(
                collect_text(&mut input, &ctx(100, 100), key(), || Ok(())),
                Err(ExtractError::Failed(_))
            ));
        }
        let mut input = Fixture {
            chunks: VecDeque::from([
                text_chunk(Boundary::None),
                Err(failed("GetChunk", "missing embedded filter")),
            ]),
            text: VecDeque::from([part("prefix", TextEnd::Last)]),
        };
        assert!(collect_text(&mut input, &ctx(100, 100), key(), || Ok(())).is_err());
    }

    #[test]
    fn malformed_unicode_and_exhausted_work_remain_failures() {
        for units in [vec![0xdc00], vec![0xd800], vec![0xd800, u16::from(b'x')]] {
            let mut input = Fixture {
                chunks: VecDeque::from([text_chunk(Boundary::None), Ok(None)]),
                text: VecDeque::from([Ok((units, TextEnd::Last))]),
            };
            assert!(collect_text(&mut input, &ctx(100, 100), key(), || Ok(())).is_err());
        }
        let mut input = Fixture {
            chunks: VecDeque::from([Ok(Some(Chunk {
                is_text: false,
                boundary: Boundary::None,
            }))]),
            text: VecDeque::new(),
        };
        let mut checks = 0;
        assert!(
            collect_text(&mut input, &ctx(100, 100), key(), || {
                checks += 1;
                if checks >= 2 {
                    Err(failed("budget", "expired after native call"))
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        assert_eq!(checks, 2);
    }

    #[test]
    fn registration_resolution_is_bounded_and_never_opens_the_input_path() {
        let persistent = "{11111111-2222-3333-4444-555555555555}";
        let filter = "{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}";
        let mut calls = Vec::new();
        let found = registered_filter("rtf", |path| {
            calls.push(path.to_string());
            Ok(Some(
                if calls.len() == 1 { persistent } else { filter }.into(),
            ))
        })
        .unwrap();
        assert_eq!(found, 0xaaaaaaaa_bbbb_cccc_dddd_eeeeeeeeeeee);
        assert_eq!(
            calls,
            [
                ".rtf\\PersistentHandler".to_string(),
                format!("CLSID\\{persistent}\\PersistentAddinsRegistered\\{{89BCB740-6119-101A-BCB7-00DD010655AF}}"),
            ]
        );
        let mut step = 0;
        registered_filter("msg", |_| {
            let values = [
                None,
                Some("Outlook.File.msg.15"),
                Some(filter),
                Some(persistent),
                Some(filter),
            ];
            let value = values[step].map(str::to_string);
            step += 1;
            Ok(value)
        })
        .unwrap();
        assert_eq!(step, 5);
        assert!(matches!(
            registered_filter("odt", |_| Ok(None)),
            Err(ExtractError::Unsupported(_))
        ));
        assert!(matches!(
            registered_filter("rtf", |_| Err(failed("registry", "denied"))),
            Err(ExtractError::Failed(_))
        ));
        assert!(matches!(
            registered_filter("..\\rtf", |_| {
                panic!("untrusted extension must never enter lookup")
            }),
            Err(ExtractError::Unsupported(_))
        ));
        for bad in [
            "Outlook.File",
            "{}",
            "{00000000-0000-0000-0000-00000000000z}",
            "{00000000/0000-0000-0000-000000000000}",
        ] {
            assert!(guid_number(bad).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_empty_and_oversized_snapshots_do_not_require_filter_registration() {
        let extractor = IFilterExtractor::new();
        let context = ctx(0, 0);
        let empty = extractor.extract_bytes(&context, key(), b"").unwrap();
        assert_eq!(empty.key, key());
        assert!(empty.text.is_empty());
        assert!(!empty.truncated);
        assert!(matches!(
            extractor.extract_bytes(&context, key(), b"x"),
            Err(ExtractError::FileTooLarge {
                bytes: 1,
                max_bytes: 0,
            })
        ));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires a machine-registered RTF IFilter supporting IPersistStream"]
    fn native_registered_rtf_reads_verified_bytes_and_not_a_replaced_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.rtf");
        let original = br"{\rtf1\ansi verifiedstreamneedle\par secondline}";
        std::fs::write(&path, original).unwrap();
        let handle = std::fs::File::open(&path).unwrap();
        std::fs::rename(&path, dir.path().join("retained-original.rtf")).unwrap();
        std::fs::write(&path, br"{\rtf1\ansi wrongreplacementneedle}").unwrap();
        let ctx = ExtractContext {
            path: path.to_str().unwrap(),
            ..ctx(4096, 4096)
        };
        let extractor = IFilterExtractor::new();
        // Exercise the factory actually used by index-worker, not only the
        // standalone adapter. Native fallback must work without Extractous.
        let stack = crate::ExtractorStack::with_extractous_enabled(false);
        for output in [
            extractor.extract_file(&ctx, key(), &handle).unwrap(),
            extractor.extract_bytes(&ctx, key(), original).unwrap(),
            stack.extract_file(key(), &ctx, &handle).unwrap(),
            stack.extract_bytes(key(), &ctx, original).unwrap(),
        ] {
            assert_eq!(output.key, key());
            assert!(output.text.contains("verifiedstreamneedle"));
            assert!(!output.text.contains("wrongreplacementneedle"));
            assert!(!output.truncated);
        }
        let limited = ExtractContext {
            max_chars: 4,
            ..ctx.clone()
        };
        let output = extractor.extract_bytes(&limited, key(), original).unwrap();
        assert_eq!(output.text.chars().count(), 4);
        assert!(output.truncated);
        let oversized = ExtractContext {
            max_bytes: original.len() - 1,
            ..ctx.clone()
        };
        assert!(matches!(
            extractor.extract_bytes(&oversized, key(), original),
            Err(ExtractError::FileTooLarge { .. })
        ));
    }
}

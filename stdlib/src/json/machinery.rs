// spell-checker:ignore LOJKINE
// derived from https://github.com/lovasoa/json_in_type

// BSD 2-Clause License
//
// Copyright (c) 2018, Ophir LOJKINE
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// * Redistributions of source code must retain the above copyright notice, this
//   list of conditions and the following disclaimer.
//
// * Redistributions in binary form must reproduce the above copyright notice,
//   this list of conditions and the following disclaimer in the documentation
//   and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
// FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
// CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::io;

use itertools::Itertools;
use rustpython_common::wtf8::{CodePoint, Wtf8, Wtf8Buf};

static ESCAPE_CHARS: [&str; 0x20] = [
    "\\u0000", "\\u0001", "\\u0002", "\\u0003", "\\u0004", "\\u0005", "\\u0006", "\\u0007", "\\b",
    "\\t", "\\n", "\\u000", "\\f", "\\r", "\\u000e", "\\u000f", "\\u0010", "\\u0011", "\\u0012",
    "\\u0013", "\\u0014", "\\u0015", "\\u0016", "\\u0017", "\\u0018", "\\u0019", "\\u001a",
    "\\u001", "\\u001c", "\\u001d", "\\u001e", "\\u001f",
];

// This bitset represents which bytes can be copied as-is to a JSON string (0)
// And which one need to be escaped (1)
// The characters that need escaping are 0x00 to 0x1F, 0x22 ("), 0x5C (\), 0x7F (DEL)
// Non-ASCII unicode characters can be safely included in a JSON string
#[allow(clippy::unusual_byte_groupings)] // it's groups of 16, come on clippy
static NEEDS_ESCAPING_BITSET: [u64; 4] = [
    //fedcba9876543210_fedcba9876543210_fedcba9876543210_fedcba9876543210
    0b0000000000000000_0000000000000100_1111111111111111_1111111111111111, // 3_2_1_0
    0b1000000000000000_0000000000000000_0001000000000000_0000000000000000, // 7_6_5_4
    0b0000000000000000_0000000000000000_0000000000000000_0000000000000000, // B_A_9_8
    0b0000000000000000_0000000000000000_0000000000000000_0000000000000000, // F_E_D_C
];

#[inline(always)]
fn json_escaped_char(c: u8) -> Option<&'static str> {
    let bitset_value = NEEDS_ESCAPING_BITSET[(c / 64) as usize] & (1 << (c % 64));
    if bitset_value == 0 {
        None
    } else {
        Some(match c {
            x if x < 0x20 => ESCAPE_CHARS[c as usize],
            b'\\' => "\\\\",
            b'\"' => "\\\"",
            0x7F => "\\u007f",
            _ => unreachable!(),
        })
    }
}

pub fn write_json_string<W: io::Write>(s: &str, ascii_only: bool, w: &mut W) -> io::Result<()> {
    w.write_all(b"\"")?;
    let mut write_start_idx = 0;
    let bytes = s.as_bytes();
    if ascii_only {
        for (idx, c) in s.char_indices() {
            if c.is_ascii() {
                if let Some(escaped) = json_escaped_char(c as u8) {
                    w.write_all(&bytes[write_start_idx..idx])?;
                    w.write_all(escaped.as_bytes())?;
                    write_start_idx = idx + 1;
                }
            } else {
                w.write_all(&bytes[write_start_idx..idx])?;
                write_start_idx = idx + c.len_utf8();
                // codepoints outside the BMP get 2 '\uxxxx' sequences to represent them
                for point in c.encode_utf16(&mut [0; 2]) {
                    write!(w, "\\u{point:04x}")?;
                }
            }
        }
    } else {
        for (idx, c) in s.bytes().enumerate() {
            if let Some(escaped) = json_escaped_char(c) {
                w.write_all(&bytes[write_start_idx..idx])?;
                w.write_all(escaped.as_bytes())?;
                write_start_idx = idx + 1;
            }
        }
    }
    w.write_all(&bytes[write_start_idx..])?;
    w.write_all(b"\"")
}

#[derive(Debug)]
pub struct DecodeError {
    pub msg: String,
    pub pos: usize,
}
impl DecodeError {
    fn new(msg: impl Into<String>, pos: usize) -> Self {
        let msg = msg.into();
        Self { msg, pos }
    }
}

enum StrOrChar<'a> {
    Str(&'a Wtf8),
    Char(CodePoint),
}
impl StrOrChar<'_> {
    const fn len(&self) -> usize {
        match self {
            StrOrChar::Str(s) => s.len(),
            StrOrChar::Char(c) => c.len_wtf8(),
        }
    }
}
pub fn scanstring<'a>(
    s: &'a Wtf8,
    end: usize,
    strict: bool,
) -> Result<(Wtf8Buf, usize), DecodeError> {
    let mut chunks: Vec<StrOrChar<'a>> = Vec::new();
    let mut output_len = 0usize;
    let mut push_chunk = |chunk: StrOrChar<'a>| {
        output_len += chunk.len();
        chunks.push(chunk);
    };
    let unterminated_err = || DecodeError::new("Unterminated string starting at", end - 1);
    let mut chars = s.code_point_indices().enumerate().skip(end).peekable();
    let &(_, (mut chunk_start, _)) = chars.peek().ok_or_else(unterminated_err)?;
    while let Some((char_i, (byte_i, c))) = chars.next() {
        match c.to_char_lossy() {
            '"' => {
                push_chunk(StrOrChar::Str(&s[chunk_start..byte_i]));
                let mut out = Wtf8Buf::with_capacity(output_len);
                for x in chunks {
                    match x {
                        StrOrChar::Str(s) => out.push_wtf8(s),
                        StrOrChar::Char(c) => out.push(c),
                    }
                }
                return Ok((out, char_i + 1));
            }
            '\\' => {
                push_chunk(StrOrChar::Str(&s[chunk_start..byte_i]));
                let (_, (_, c)) = chars.next().ok_or_else(unterminated_err)?;
                let esc = match c.to_char_lossy() {
                    '"' => "\"",
                    '\\' => "\\",
                    '/' => "/",
                    'b' => "\x08",
                    'f' => "\x0c",
                    'n' => "\n",
                    'r' => "\r",
                    't' => "\t",
                    'u' => {
                        let mut uni = decode_unicode(&mut chars, char_i)?;
                        chunk_start = byte_i + 6;
                        if let Some(lead) = uni.to_lead_surrogate() {
                            // uni is a surrogate -- try to find its pair
                            let mut chars2 = chars.clone();
                            if let Some(((pos2, _), (_, _))) = chars2
                                .next_tuple()
                                .filter(|((_, (_, c1)), (_, (_, c2)))| *c1 == '\\' && *c2 == 'u')
                            {
                                let uni2 = decode_unicode(&mut chars2, pos2)?;
                                if let Some(trail) = uni2.to_trail_surrogate() {
                                    // ok, we found what we were looking for -- \uXXXX\uXXXX, both surrogates
                                    uni = lead.merge(trail).into();
                                    chunk_start = pos2 + 6;
                                    chars = chars2;
                                }
                            }
                        }
                        push_chunk(StrOrChar::Char(uni));
                        continue;
                    }
                    _ => {
                        return Err(DecodeError::new(format!("Invalid \\escape: {c:?}"), char_i));
                    }
                };
                chunk_start = byte_i + 2;
                push_chunk(StrOrChar::Str(esc.as_ref()));
            }
            '\x00'..='\x1f' if strict => {
                return Err(DecodeError::new(
                    format!("Invalid control character {c:?} at"),
                    char_i,
                ));
            }
            _ => {}
        }
    }
    Err(unterminated_err())
}

#[inline]
fn decode_unicode<I>(it: &mut I, pos: usize) -> Result<CodePoint, DecodeError>
where
    I: Iterator<Item = (usize, (usize, CodePoint))>,
{
    let err = || DecodeError::new("Invalid \\uXXXX escape", pos);
    let mut uni = 0;
    for x in (0..4).rev() {
        let (_, (_, c)) = it.next().ok_or_else(err)?;
        let d = c.to_char().and_then(|c| c.to_digit(16)).ok_or_else(err)? as u16;
        uni += d * 16u16.pow(x);
    }
    Ok(uni.into())
}

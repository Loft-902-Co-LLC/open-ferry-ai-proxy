// Ported from CLIProxyAPI internal/translator/common/apply_patch_input_test.go
// and apply_patch_events_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use crate::apply_patch::wrap_input;

/// Pushes `fragments`, checking each delta, then finishes with `arguments`.
fn check_fragments(fragments: &[&[u8]], arguments: &[u8], want: &str) {
    let mut decoder = InputDecoder::default();
    let mut output = String::new();
    for (i, fragment) in fragments.iter().enumerate() {
        let delta = decoder
            .push(fragment)
            .unwrap_or_else(|error| panic!("fragment {i}: {error}"));
        output.push_str(&delta);
        assert_eq!(decoder.input(), output, "fragment {i}");
    }
    let tail = decoder.finish(arguments).unwrap();
    output.push_str(&tail);
    assert_eq!(output, want);
    assert_eq!(decoder.input(), want);
}

#[test]
fn every_split_decodes_the_patch() {
    let patch = "*** Begin Patch\n*** Add File: a.txt\n+中文 \\\"\n*** End Patch\n";
    let arguments = wrap_input(patch);
    let arguments = arguments.as_bytes();
    for split in 0..=arguments.len() {
        check_fragments(
            &[&arguments[..split], &arguments[split..]],
            arguments,
            patch,
        );
    }
}

#[test]
fn string_fragments() {
    let cases: [(&str, &str); 7] = [
        (r#"{"input":""}"#, ""),
        (
            " \t\r\n{ \n\"input\" \t: \"  line  \\n\\t next\\r\\n\" \r}\n\t",
            "  line  \n\t next\r\n",
        ),
        (
            r#"{"input":"\"\\\/\b\f\n\r\t\u0000\u0041\u4e2d\u6587"}"#,
            "\"\\/\u{8}\u{c}\n\r\t\u{0}A中文",
        ),
        (r#"{"in\u0070ut":"patch"}"#, "patch"),
        (r#"{"input":"before\uD83D\uDE00after"}"#, "before😀after"),
        (
            r#"{"input":"\ud800\udc00\uDBFF\uDFFF\uD7FF\uE000"}"#,
            "\u{10000}\u{10ffff}\u{d7ff}\u{e000}",
        ),
        (r#"{"input":"¢中文😀�"}"#, "¢中文😀�"),
    ];
    for (arguments, want) in cases {
        let arguments = arguments.as_bytes();
        for split in 0..=arguments.len() {
            check_fragments(&[&arguments[..split], &arguments[split..]], arguments, want);
            // The final arguments can complete a pending escape or character.
            check_fragments(&[&arguments[..split]], arguments, want);
        }
        let bytes: Vec<&[u8]> = arguments.chunks(1).collect();
        check_fragments(&bytes, arguments, want);
    }
}

#[test]
fn previews_input_before_the_json_closes() {
    let fragments: [(&[u8], &str); 8] = [
        (
            br#"{"input":"*** Begin Patch\n*** Add File: a.txt\n+  "#,
            "*** Begin Patch\n*** Add File: a.txt\n+  ",
        ),
        (&[0xe4], ""),
        (&[0xb8], ""),
        (&[0xad, b'\\'], "中"),
        (b"uD8", ""),
        (b"3D", ""),
        (br"\uDE", ""),
        (br"00\n*** End Patch\n", "😀\n*** End Patch\n"),
    ];
    let mut decoder = InputDecoder::default();
    for (i, (fragment, want)) in fragments.into_iter().enumerate() {
        assert_eq!(decoder.push(fragment).as_deref(), Ok(want), "fragment {i}");
    }
    let want = "*** Begin Patch\n*** Add File: a.txt\n+  中😀\n*** End Patch\n";
    assert_eq!(decoder.input(), want);
    assert_eq!(decoder.finish(wrap_input(want)).as_deref(), Ok(""));
}

#[test]
fn rejects_invalid_arguments() {
    let cases: [&[u8]; 36] = [
        b"",
        b"[]",
        b"{}",
        br#"{"patch":"x"}"#,
        br#"{input:"x"}"#,
        br#"{"in\qput":"x"}"#,
        br#"{"input" "x"}"#,
        br#"{"input":42}"#,
        br#"{"input":null}"#,
        br#"{"input":true}"#,
        br#"{"input":{}}"#,
        br#"{"input":[]}"#,
        br#"{"input":"x","input":"y"}"#,
        br#"{"input":"x","extra":"y"}"#,
        br#"{"input":"x",}"#,
        br#"{"input":"x"}{}"#,
        b"\x0b{\"input\":\"x\"}",
        br#"{"input":"\q"}"#,
        br#"{"input":"\u12G4"}"#,
        br#"{"input":"\u123"}"#,
        br#"{"input":"x\"#,
        b"{\"input\":\"x\ny\"}",
        b"{\"input\":\"x\x00y\"}",
        br#"{"input":"\uDE00"}"#,
        br#"{"input":"\uD83D"}"#,
        br#"{"input":"\uD83Dx"}"#,
        br#"{"input":"\uD83D\n"}"#,
        br#"{"input":"\uD83DA"}"#,
        br#"{"input":"\uD83D\uD83D"}"#,
        b"{\"input\":\"\xe4A\"}",
        b"{\"input\":\"\xe4\xb8\"}",
        b"{\"input\":\"\xc0\xaf\"}",
        b"{\"input\":\"\xed\xa0\x80\"}",
        b"{\"input\":\"\xf4\x90\x80\x80\"}",
        br#"{"input":"x"#,
        br#"{"input":"x""#,
    ];
    for arguments in cases {
        let shown = String::from_utf8_lossy(arguments);
        for split in 0..=arguments.len() {
            let mut decoder = InputDecoder::default();
            for fragment in [&arguments[..split], &arguments[split..]] {
                if decoder.push(fragment).is_err() {
                    break;
                }
            }
            assert!(
                decoder.finish(arguments).is_err(),
                "split {split} accepted {shown:?}"
            );
        }
        let mut decoder = InputDecoder::default();
        for byte in arguments.chunks(1) {
            if decoder.push(byte).is_err() {
                break;
            }
        }
        assert!(
            decoder.finish(arguments).is_err(),
            "bytes accepted {shown:?}"
        );
        assert!(
            InputDecoder::default().finish(arguments).is_err(),
            "finish alone accepted {shown:?}"
        );
    }
}

#[test]
fn an_invalid_value_fails_at_once() {
    for fragment in [
        r#"{"input":4"#,
        r#"{"input":n"#,
        r#"{"input":t"#,
        r#"{"input":["#,
        r#"{"input":{"#,
        r#"{"input":"\q"#,
        r#"{"input":"\u12G"#,
        r#"{"input":"\uDE00"#,
        r#"{"input":"\uD83Dx"#,
    ] {
        assert!(
            InputDecoder::default().push(fragment).is_err(),
            "{fragment}"
        );
    }
}

#[test]
fn invalid_pending_characters_add_nothing() {
    let cases: [(&[u8], &[u8]); 8] = [
        (br"\uD83D", br#""}"#),
        (br"\uD83D", b"x"),
        (br"\uD83D\", b"n"),
        (br"\uD83D\u00", b"41"),
        (br"\", b"q"),
        (br"\u12", b"G"),
        (&[0xe4], b"x"),
        (&[0xe4, 0xb8], br#""}"#),
    ];
    for (pending, continuation) in cases {
        let mut decoder = InputDecoder::default();
        let mut first = br#"{"input":"safe"#.to_vec();
        first.extend_from_slice(pending);
        assert_eq!(decoder.push(&first).as_deref(), Ok("safe"));
        assert!(decoder.push(continuation).is_err());
        assert_eq!(decoder.input(), "safe");
    }
}

#[test]
fn finishing_again_adds_nothing() {
    let arguments = wrap_input("  first\nsecond  \n");
    for (push, want) in [
        ("", "  first\nsecond  \n"),
        (r#"{"input":"  first\n"#, "second  \n"),
        (arguments.as_str(), ""),
    ] {
        let mut decoder = InputDecoder::default();
        decoder.push(push).unwrap();
        assert_eq!(decoder.finish(&arguments).as_deref(), Ok(want));
        assert_eq!(decoder.input(), "  first\nsecond  \n");
        for _ in 0..3 {
            assert_eq!(decoder.finish(&arguments).as_deref(), Ok(""));
        }
        assert!(
            decoder
                .finish(wrap_input("  first\nsecond  \nmore"))
                .is_err()
        );
    }
}

#[test]
fn rejects_a_final_input_that_conflicts_with_the_stream() {
    for last in ["other", "pre", "prefix changed"] {
        let mut decoder = InputDecoder::default();
        assert_eq!(
            decoder.push(r#"{"input":"prefix original"#).as_deref(),
            Ok("prefix original")
        );
        assert!(decoder.finish(wrap_input(last)).is_err(), "{last}");
        assert_eq!(decoder.input(), "prefix original");
    }
}

#[test]
fn an_error_is_final() {
    let mut decoder = InputDecoder::default();
    let error = decoder.push(r#"{"input":"safe\uD83DA"#).unwrap_err();
    assert_eq!(decoder.input(), "safe");
    assert_eq!(decoder.push(r#"suffix"}"#), Err(error.clone()));
    assert_eq!(decoder.finish(wrap_input("safe")), Err(error));
}

#[test]
fn a_finish_error_is_final() {
    for arguments in [
        r#"{"input":null}"#,
        r#"{"input":"wrong prefix"}"#,
        r#"{"input":"safe\uD83D"}"#,
    ] {
        let mut decoder = InputDecoder::default();
        assert_eq!(decoder.push(r#"{"input":"safe"#).as_deref(), Ok("safe"));
        let error = decoder.finish(arguments).unwrap_err();
        assert_eq!(decoder.finish(r#"{"input":"safe"}"#), Err(error));
        assert_eq!(decoder.input(), "safe");
    }
}

#[test]
fn accepts_an_equivalent_final_encoding() {
    let mut decoder = InputDecoder::default();
    assert_eq!(decoder.push("{\"input\":\"中\\n").as_deref(), Ok("中\n"));
    assert_eq!(
        decoder.finish(r#"{"in\u0070ut":"\u4e2d\n😀"}"#).as_deref(),
        Ok("😀")
    );
    assert_eq!(
        decoder
            .finish(" \n{\"input\":\"中\\n\\uD83D\\uDE00\"}\t")
            .as_deref(),
        Ok("")
    );
}

#[test]
fn rejects_a_push_after_finishing() {
    let mut decoder = InputDecoder::default();
    decoder.finish(r#"{"input":"patch"}"#).unwrap();
    assert!(decoder.push("more").is_err());
    assert_eq!(decoder.input(), "patch");
}

#[test]
fn interleaved_calls_decode_independently() {
    let mut first = CallState::new("item_1".into(), "call_1".into(), 2);
    let mut second = CallState::new("item_2".into(), "call_2".into(), 4);
    assert_eq!(
        first.push_arguments(r#"{"input":"first\uD83D"#).as_deref(),
        Ok("first")
    );
    assert_eq!(
        second.push_arguments(b"{\"input\":\"second\xe4").as_deref(),
        Ok("second")
    );
    assert_eq!(first.push_arguments(r"\uDE00\n").as_deref(), Ok("😀\n"));
    assert_eq!(second.push_arguments([0xb8, 0xad]).as_deref(), Ok("中"));
    assert_eq!(
        first.finish_arguments(wrap_input("first😀\nlast")),
        Ok(("last".to_owned(), "first😀\nlast".to_owned()))
    );
    assert_eq!(
        second.finish_arguments(wrap_input("second中")),
        Ok((String::new(), "second中".to_owned()))
    );
    assert_eq!(
        (
            first.item_id.as_str(),
            first.call_id.as_str(),
            first.output_index
        ),
        ("item_1", "call_1", 2)
    );
}

#[test]
fn a_failed_call_leaves_others_alone() {
    let mut bad = CallState::default();
    let mut good = CallState::default();
    assert!(bad.push_arguments(r#"{"input":null"#).is_err());
    assert_eq!(
        good.push_arguments(r#"{"input":"good"#).as_deref(),
        Ok("good")
    );
    assert!(bad.finish_arguments(r#"{"input":"bad"}"#).is_err());
    assert_eq!(bad.input(), "");
    for want in [" tail", ""] {
        assert_eq!(
            good.finish_arguments(r#"{"input":"good tail"}"#),
            Ok((want.to_owned(), "good tail".to_owned()))
        );
    }
}

#[test]
fn byte_at_a_time_calls_decode() {
    for index in 0..16 {
        let mut state = CallState::new(format!("item_{index}"), format!("call_{index}"), index);
        let want = format!("*** Begin Patch\n+  call {index} 中文😀  \n*** End Patch\n");
        let arguments = wrap_input(&want);
        let mut output = String::new();
        for byte in arguments.as_bytes().chunks(1) {
            output.push_str(&state.push_arguments(byte).unwrap());
        }
        let (tail, input) = state.finish_arguments(&arguments).unwrap();
        output.push_str(&tail);
        assert_eq!(
            (output.as_str(), input.as_str()),
            (want.as_str(), want.as_str())
        );
        let done = state.input_done(&input, index);
        assert_eq!(done["call_id"], state.call_id);
    }
}

#[test]
fn event_payloads() {
    let state = CallState::new("item_\"中".into(), "call_\\1".into(), 3);
    let text = "*** Begin Patch\n+  中文 \\\"  \n*** End Patch\n";
    assert_eq!(
        state.input_delta(text, 11).to_string(),
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"item_\"中","call_id":"call_\\1","output_index":3,"sequence_number":11,"delta":"*** Begin Patch\n+  中文 \\\"  \n*** End Patch\n"}"#
    );
    assert_eq!(
        state.input_done(text, 12).to_string(),
        r#"{"type":"response.custom_tool_call_input.done","item_id":"item_\"中","call_id":"call_\\1","output_index":3,"sequence_number":12,"input":"*** Begin Patch\n+  中文 \\\"  \n*** End Patch\n"}"#
    );
    assert_eq!(
        CallState::default().input_delta("", 0).to_string(),
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"","call_id":"","output_index":0,"sequence_number":0,"delta":""}"#
    );
}

#[test]
fn failure_says_nothing_about_the_arguments() {
    assert_eq!(
        failure("resp_\"1", 17).to_string(),
        r#"{"type":"response.failed","sequence_number":17,"response":{"id":"resp_\"1","object":"response","status":"failed","error":{"type":"server_error","code":"invalid_tool_arguments","message":"Invalid apply_patch tool arguments received from upstream.","param":null}}}"#
    );
}

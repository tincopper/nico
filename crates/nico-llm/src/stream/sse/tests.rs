//! The framing's own invariants: every chunk boundary, every line ending and every ignored field.

use super::{DONE, Decoder, Event, MAX_EVENT_BYTES, SseError, encode_done, encode_event};

fn event(name: Option<&str>, data: &str) -> Event {
    Event {
        name: name.map(str::to_owned),
        data: data.to_owned(),
    }
}

/// Feeds `stream` in fixed-size chunks and returns every event, in order.
fn decode_in_chunks(stream: &[u8], chunk: usize) -> Vec<Event> {
    let mut decoder = Decoder::new();
    let mut events = Vec::new();
    for piece in stream.chunks(chunk) {
        events.extend(decoder.push(piece).expect("every chunk boundary is safe"));
    }
    events.extend(decoder.finish().expect("the stream ends cleanly"));
    events
}

/// A stream with every shape the framing has to survive.
fn sample() -> Vec<u8> {
    let mut stream = Vec::new();
    stream.extend_from_slice(b": keep-alive\n\n");
    stream.extend_from_slice(b"event: message\nid: 7\nretry: 100\n");
    stream.extend_from_slice(b"data: {\"text\":\"h\xc3\xa9llo \xe4\xb8\x96\xe7\x95\x8c\"}\n\n");
    stream.extend_from_slice(b"data: two\n");
    stream.extend_from_slice(b"\n");
    stream.extend_from_slice(b"data: [DONE]\n\n");
    stream
}

#[test]
fn the_same_stream_decodes_identically_at_every_chunk_boundary() {
    let stream = sample();
    let whole = decode_in_chunks(&stream, stream.len());
    assert_eq!(
        whole,
        vec![
            event(Some("message"), "{\"text\":\"héllo 世界\"}"),
            event(None, "two"),
            event(None, DONE),
        ],
        "the sample must decode to the expected events"
    );
    for chunk in 1..=stream.len() {
        assert_eq!(
            decode_in_chunks(&stream, chunk),
            whole,
            "chunk size {chunk} changed the event sequence"
        );
    }
}

#[test]
fn a_multibyte_character_split_across_chunks_survives() {
    let stream = "data: héllo 世界\n\n".as_bytes();
    for chunk in 1..stream.len() {
        assert_eq!(
            decode_in_chunks(stream, chunk),
            vec![event(None, "héllo 世界")],
            "chunk size {chunk} split a character"
        );
    }
}

#[test]
fn every_line_ending_dispatches_the_same_event() {
    for stream in [
        b"data: one\n\ndata: two\n\n".as_slice(),
        b"data: one\r\n\r\ndata: two\r\n\r\n",
        b"data: one\r\rdata: two\r\r",
        b"data: one\r\n\ndata: two\n\r\n",
    ] {
        assert_eq!(
            decode_in_chunks(stream, 1),
            vec![event(None, "one"), event(None, "two")],
            "the lines of {:?} were not split correctly",
            String::from_utf8_lossy(stream)
        );
    }
}

#[test]
fn a_trailing_carriage_return_terminates_the_last_line() {
    let mut decoder = Decoder::new();
    assert!(
        decoder
            .push(b"data: one\n\r")
            .expect("the hold-back is not an error")
            .is_empty(),
        "a `\\r` at the end may still become `\\r\\n`"
    );
    assert_eq!(
        decoder.finish().expect("the stream ends cleanly"),
        vec![event(None, "one")]
    );
}

#[test]
fn a_frame_needs_a_blank_line_to_be_dispatched() {
    let mut decoder = Decoder::new();
    assert!(
        decoder
            .push(b"data: one\n")
            .expect("a partial frame is not an error")
            .is_empty()
    );
    assert_eq!(
        decoder.finish().expect_err("the frame never closed"),
        SseError::Incomplete
    );
}

#[test]
fn comments_and_unused_fields_are_not_events() {
    let mut decoder = Decoder::new();
    let events = decoder
        .push(b": heartbeat\n\nid: 7\nretry: 100\n\n")
        .expect("ignored fields are well formed");
    assert!(events.is_empty(), "neither line starts an event");
    assert!(decoder.finish().expect("nothing is left").is_empty());
}

#[test]
fn multiple_data_lines_are_joined_with_a_newline() {
    assert_eq!(
        decode_in_chunks(b"data: one\ndata: two\n\n", 1),
        vec![event(None, "one\ntwo")]
    );
}

#[test]
fn several_events_in_one_chunk_keep_their_order() {
    let mut decoder = Decoder::new();
    let events = decoder
        .push(b"data: a\n\ndata: b\n\ndata: c\n\n")
        .expect("three frames are well formed");
    assert_eq!(
        events,
        vec![event(None, "a"), event(None, "b"), event(None, "c")]
    );
}

#[test]
fn an_empty_payload_is_an_event_but_a_bare_blank_line_is_not() {
    let mut decoder = Decoder::new();
    let events = decoder
        .push(b"\n\ndata:\n\n")
        .expect("both are well formed");
    assert_eq!(events, vec![event(None, "")]);
}

#[test]
fn a_byte_order_mark_is_not_part_of_the_first_field_name() {
    let stream = b"\xef\xbb\xbfdata: one\n\n";
    for chunk in 1..=stream.len() {
        assert_eq!(
            decode_in_chunks(stream, chunk),
            vec![event(None, "one")],
            "chunk size {chunk} split the mark"
        );
    }
}

#[test]
fn an_event_that_never_ends_is_refused() {
    let mut decoder = Decoder::new();
    let error = decoder
        .push(&vec![b'x'; MAX_EVENT_BYTES + 1])
        .expect_err("an unbounded event must be refused");
    assert_eq!(
        error,
        SseError::TooLarge {
            limit: MAX_EVENT_BYTES
        }
    );
}

#[test]
fn an_event_that_keeps_opening_lines_is_refused_too() {
    // Each line is consumed as it arrives, so a bound on the buffered bytes would never fire: the
    // bound has to count the frame's accumulated payload instead.
    let line = format!("data: {}\n", "x".repeat(4096));
    let mut decoder = Decoder::new();
    let mut refusal = None;
    for _ in 0..(MAX_EVENT_BYTES / line.len() + 2) {
        match decoder.push(line.as_bytes()) {
            Ok(events) => assert!(events.is_empty(), "the frame never closed"),
            Err(error) => {
                refusal = Some(error);
                break;
            }
        }
    }
    assert_eq!(
        refusal,
        Some(SseError::TooLarge {
            limit: MAX_EVENT_BYTES
        }),
        "the bound must count the whole event, not one line of it"
    );
    assert!(
        decoder
            .finish()
            .expect("the refusal dropped the frame")
            .is_empty(),
        "a refused event leaves nothing buffered"
    );
}

#[test]
fn the_bound_is_per_event_not_per_chunk() {
    let payload = "x".repeat(MAX_EVENT_BYTES / 8);
    let frame = format!("data: {payload}\n\n");
    let mut decoder = Decoder::new();
    let events = decoder
        .push(frame.repeat(8).as_bytes())
        .expect("every event is far below the bound");
    assert_eq!(events.len(), 8, "a large chunk may hold many small events");
    assert_eq!(events[0].data.len(), payload.len());
}

#[test]
fn a_line_that_is_not_utf8_is_refused() {
    let mut decoder = Decoder::new();
    assert_eq!(
        decoder.push(b"data: \xff\n\n").expect_err("invalid UTF-8"),
        SseError::NotUtf8
    );
    assert!(
        decoder
            .finish()
            .expect("the refusal dropped the frame")
            .is_empty(),
        "a refused stream leaves nothing buffered"
    );
}

#[test]
fn the_encoder_frames_every_line_and_marks_the_terminator() {
    assert_eq!(encode_event("{\"a\":1}"), b"data: {\"a\":1}\n\n");
    assert_eq!(encode_event("one\ntwo"), b"data: one\ndata: two\n\n");
    assert_eq!(encode_done(), b"data: [DONE]\n\n");

    let mut stream = encode_event("one\ntwo");
    stream.extend_from_slice(&encode_done());
    assert_eq!(
        decode_in_chunks(&stream, 3),
        vec![event(None, "one\ntwo"), event(None, DONE)]
    );
}

// crates/asterisk-controller/src/playout.rs

//! The playback queue of one participant.
//!
//! The application queues segments of audio; Asterisk's media channel plays
//! what it is given at the pace of the call. Between the two stands this
//! queue. It knows nothing of connections: it is told what happened —
//! a segment was queued, audio arrived, Asterisk reached a mark — and
//! answers with what to hand to Asterisk and what to tell the application.
//!
//! Three things are decided here and nowhere else.
//!
//! **What "delivered" means.** A segment is delivered when Asterisk says it
//! has handed the last of its audio to the call. Asterisk says so by
//! reaching a mark the queue put behind that audio; nothing is computed
//! from a clock.
//!
//! **How much Asterisk is given at once.** Asterisk's own queue is short and
//! drops what does not fit without a word. So the queue hands over a few
//! seconds at most and more only as marks come back; the rest waits here.
//!
//! **What a flush reports.** Asterisk is paused, asked how much it still
//! holds, and only then told to drop it: the delivered part of the segment
//! that was playing is what Asterisk itself counted.

use std::{collections::VecDeque, fmt};

use node_protocol::messages::SegmentId;

/// Bytes of one frame handed to Asterisk: twenty milliseconds of audio.
pub const FRAME_BYTES: usize = 640;
/// Milliseconds of audio in one frame.
const FRAME_MS: u64 = 20;
/// Bytes of one millisecond of audio.
const BYTES_PER_MS: u64 = 32;
/// Most frames handed to Asterisk and not yet reported as played. Asterisk
/// holds a thousand and starts refusing at nine hundred; this stays far
/// below, so its own flow control never has to speak.
const WINDOW_FRAMES: usize = 250;
/// A mark is put behind every this many frames of a segment, so that what
/// has been played is known to a fifth of a second and the window moves.
const PROGRESS_FRAMES: usize = 10;

/// What the queue hands to Asterisk's media channel, in order.
#[derive(Debug, PartialEq, Eq)]
pub enum ToMedium {
    /// One frame of audio, exactly [`FRAME_BYTES`] long.
    Audio(Vec<u8>),
    /// A mark: Asterisk reports it when it has played everything before it.
    Mark(u64),
    /// Stop playing, keeping what is queued.
    Pause,
    /// Report how many frames are still queued.
    AskStatus,
    /// Drop everything queued, marks included.
    Flush,
    /// Play again.
    Continue,
}

/// What became of a segment — what the application is told.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Its first audio was handed to the call.
    Started(SegmentId),
    /// All of its audio was handed to the call.
    Delivered(SegmentId),
    /// It was dropped; this much of it had been handed to the call.
    Dropped {
        /// The segment.
        segment: SegmentId,
        /// Milliseconds of it that were handed to the call.
        delivered_ms: u64,
    },
}

/// Why a segment was not queued.
#[derive(Debug, PartialEq, Eq)]
pub struct AlreadyQueued;

impl fmt::Display for AlreadyQueued {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a segment of this name is still in the queue")
    }
}

impl std::error::Error for AlreadyQueued {}

/// Asterisk reported a mark other than the next one the queue expects.
#[derive(Debug, PartialEq, Eq)]
pub struct UnexpectedMark(pub u64);

impl fmt::Display for UnexpectedMark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mark {} is not the next one handed over", self.0)
    }
}

impl std::error::Error for UnexpectedMark {}

struct Segment {
    /// The application's name for it.
    id: SegmentId,
    /// Distinguishes it from an earlier segment of the same name.
    serial: u64,
    /// Audio received and not yet handed to Asterisk.
    waiting: VecDeque<u8>,
    /// All audio received, in bytes.
    received_bytes: u64,
    /// The application has sent the last of its audio.
    closed: bool,
    /// How far handing it to Asterisk has got.
    handing: Handing,
    /// The application has been told it started.
    started: bool,
    /// Frames handed to Asterisk.
    handed_frames: u64,
    /// Frames Asterisk has reported as played.
    played_frames: u64,
}

impl Segment {
    /// Milliseconds known to have been handed to the call. A last frame is
    /// padded with silence up to twenty milliseconds, and silence the
    /// application never sent is not counted as its audio.
    fn delivered_ms(&self) -> u64 {
        (self.played_frames * FRAME_MS).min(self.received_bytes / BYTES_PER_MS)
    }

    /// Whether every byte of it is known to have been handed to the call.
    fn fully_played(&self) -> bool {
        self.started && self.handing == Handing::Done && self.played_frames == self.handed_frames
    }

    fn ending(self) -> Outcome {
        if self.fully_played() {
            Outcome::Delivered(self.id)
        } else {
            Outcome::Dropped {
                delivered_ms: self.delivered_ms(),
                segment: self.id,
            }
        }
    }
}

/// How far handing a segment to Asterisk has got.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Handing {
    /// Nothing of it has been handed over.
    NotBegun,
    /// Its start mark has been handed over; audio follows.
    Begun,
    /// Its end mark has been handed over: nothing more of it follows.
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MarkKind {
    /// In front of a segment's first frame.
    Start,
    /// Behind a run of a segment's frames.
    Progress,
    /// Behind a segment's last frame.
    End,
}

struct Mark {
    id: u64,
    segment: u64,
    kind: MarkKind,
    /// Frames of the segment handed to Asterisk since its previous mark.
    frames: usize,
}

/// The playback queue of one participant.
#[derive(Default)]
pub struct Playout {
    segments: VecDeque<Segment>,
    /// Marks handed to Asterisk and not yet reported, oldest first.
    marks: VecDeque<Mark>,
    next_serial: u64,
    next_mark: u64,
    /// Frames handed to Asterisk and not yet reported as played.
    outstanding: usize,
    /// Frames of the segment being handed over since its last mark.
    unmarked: usize,
    /// A flush is under way: this many segments from the front are to be
    /// dropped once Asterisk has said how much it still holds.
    flushing: Option<usize>,
}

impl Playout {
    /// Queue a segment. Its audio follows through [`Self::audio`].
    ///
    /// # Errors
    ///
    /// A segment of the same name that is still in the queue would make every
    /// later word about that name ambiguous, so the second one is refused.
    pub fn queue(&mut self, id: SegmentId) -> Result<(), AlreadyQueued> {
        if self.segments.iter().any(|segment| segment.id == id) {
            return Err(AlreadyQueued);
        }
        self.next_serial += 1;
        self.segments.push_back(Segment {
            id,
            serial: self.next_serial,
            waiting: VecDeque::new(),
            received_bytes: 0,
            closed: false,
            handing: Handing::NotBegun,
            started: false,
            handed_frames: 0,
            played_frames: 0,
        });
        Ok(())
    }

    /// Audio of a queued segment arrived; `last` closes the segment.
    ///
    /// Audio for a segment that is not in the queue, or is already closed, is
    /// left out: the command that would have queued it was rejected, or the
    /// segment has been dropped and the application has not read that yet.
    pub fn audio(&mut self, id: &SegmentId, audio: &[u8], last: bool) {
        let open = self
            .segments
            .iter_mut()
            .find(|segment| segment.id == *id && !segment.closed);
        if let Some(segment) = open {
            segment.waiting.extend(audio);
            segment.received_bytes += audio.len() as u64;
            segment.closed = last;
        }
    }

    /// Hand to Asterisk as much as it may be given now.
    ///
    /// Segments go strictly in order; a segment whose audio has not all
    /// arrived holds back the ones behind it.
    pub fn feed(&mut self, to_medium: &mut Vec<ToMedium>) {
        if self.flushing.is_some() {
            return;
        }
        while self.outstanding < WINDOW_FRAMES {
            let Some(segment) = self
                .segments
                .iter_mut()
                .find(|segment| segment.handing != Handing::Done)
            else {
                return;
            };
            let frame = if segment.waiting.len() >= FRAME_BYTES {
                Some(segment.waiting.drain(..FRAME_BYTES).collect::<Vec<u8>>())
            } else if !segment.closed {
                // The rest of its audio is still on its way.
                return;
            } else if segment.waiting.is_empty() {
                None
            } else {
                // Its last audio falls short of a frame: silence fills it.
                let mut frame: Vec<u8> = segment.waiting.drain(..).collect();
                frame.resize(FRAME_BYTES, 0);
                Some(frame)
            };

            if segment.handing == Handing::NotBegun {
                segment.handing = Handing::Begun;
                let id = Self::mark(
                    &mut self.marks,
                    &mut self.next_mark,
                    segment.serial,
                    MarkKind::Start,
                    0,
                );
                to_medium.push(ToMedium::Mark(id));
            }
            if let Some(frame) = frame {
                to_medium.push(ToMedium::Audio(frame));
                segment.handed_frames += 1;
                self.outstanding += 1;
                self.unmarked += 1;
                if self.unmarked == PROGRESS_FRAMES {
                    let id = Self::mark(
                        &mut self.marks,
                        &mut self.next_mark,
                        segment.serial,
                        MarkKind::Progress,
                        self.unmarked,
                    );
                    self.unmarked = 0;
                    to_medium.push(ToMedium::Mark(id));
                }
            } else {
                segment.handing = Handing::Done;
                let id = Self::mark(
                    &mut self.marks,
                    &mut self.next_mark,
                    segment.serial,
                    MarkKind::End,
                    self.unmarked,
                );
                self.unmarked = 0;
                to_medium.push(ToMedium::Mark(id));
            }
        }
    }

    fn mark(
        marks: &mut VecDeque<Mark>,
        next_mark: &mut u64,
        segment: u64,
        kind: MarkKind,
        frames: usize,
    ) -> u64 {
        *next_mark += 1;
        marks.push_back(Mark {
            id: *next_mark,
            segment,
            kind,
            frames,
        });
        *next_mark
    }

    /// Asterisk reported a mark: everything in front of it has been played.
    ///
    /// # Errors
    ///
    /// Marks come back in the order they were handed over. Any other mark
    /// means the queue and Asterisk no longer agree on what was played.
    pub fn mark_reached(
        &mut self,
        id: u64,
        outcomes: &mut Vec<Outcome>,
    ) -> Result<(), UnexpectedMark> {
        let Some(mark) = self.marks.pop_front_if(|mark| mark.id == id) else {
            return Err(UnexpectedMark(id));
        };
        self.outstanding = self.outstanding.saturating_sub(mark.frames);
        let Some(position) = self
            .segments
            .iter()
            .position(|segment| segment.serial == mark.segment)
        else {
            return Ok(());
        };
        if let Some(segment) = self.segments.get_mut(position) {
            segment.played_frames += mark.frames as u64;
            if mark.kind == MarkKind::Start {
                segment.started = true;
                outcomes.push(Outcome::Started(segment.id.clone()));
            }
        }
        if mark.kind == MarkKind::End
            && let Some(segment) = self.segments.remove(position)
        {
            outcomes.push(Outcome::Delivered(segment.id));
        }
        Ok(())
    }

    /// Drop everything queued so far. Segments queued after this are kept.
    ///
    /// When Asterisk holds nothing of the queue, the segments are dropped at
    /// once. Otherwise Asterisk is paused and asked what it still holds, and
    /// the flush is finished by [`Self::status`].
    pub fn flush(&mut self, to_medium: &mut Vec<ToMedium>, outcomes: &mut Vec<Outcome>) {
        let covered = self.segments.len();
        if self.flushing.is_some() {
            // A second flush before Asterisk has answered the first: the
            // same flush, now covering what was queued in between.
            self.flushing = Some(covered);
        } else if self.marks.is_empty() && self.outstanding == 0 {
            self.drop_front(covered, outcomes);
        } else {
            self.flushing = Some(covered);
            to_medium.push(ToMedium::Pause);
            to_medium.push(ToMedium::AskStatus);
        }
    }

    /// Asterisk, paused, said how many frames it still holds: finish the flush.
    ///
    /// What it no longer holds it has played. That is credited to the
    /// segments in the order their frames were handed over, and only then
    /// is Asterisk told to drop the rest.
    pub fn status(
        &mut self,
        frames_held: usize,
        to_medium: &mut Vec<ToMedium>,
        outcomes: &mut Vec<Outcome>,
    ) {
        let Some(covered) = self.flushing.take() else {
            return;
        };
        let mut played = self.outstanding.saturating_sub(frames_held);
        for mark in self.marks.drain(..) {
            let credited = played.min(mark.frames);
            played -= credited;
            if let Some(segment) = self
                .segments
                .iter_mut()
                .find(|segment| segment.serial == mark.segment)
            {
                segment.played_frames += credited as u64;
            }
        }
        // Frames behind the last mark belong to the segment being handed over.
        let credited = played.min(self.unmarked);
        if let Some(segment) = self
            .segments
            .iter_mut()
            .find(|segment| segment.handing == Handing::Begun)
        {
            segment.played_frames += credited as u64;
        }
        self.outstanding = 0;
        self.unmarked = 0;
        to_medium.push(ToMedium::Flush);
        to_medium.push(ToMedium::Continue);
        self.drop_front(covered, outcomes);
    }

    /// The participant is gone: nothing more will be played. Every segment
    /// still queued ends here, with what is known to have been played.
    pub fn abandon(&mut self, outcomes: &mut Vec<Outcome>) {
        self.marks.clear();
        self.outstanding = 0;
        self.unmarked = 0;
        self.flushing = None;
        self.drop_front(self.segments.len(), outcomes);
    }

    fn drop_front(&mut self, count: usize, outcomes: &mut Vec<Outcome>) {
        outcomes.extend(self.segments.drain(..count).map(Segment::ending));
    }
}

#[cfg(test)]
mod tests {
    use super::{AlreadyQueued, FRAME_BYTES, Outcome, Playout, ToMedium, UnexpectedMark};
    use node_protocol::messages::SegmentId;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn segment(name: &str) -> Result<SegmentId, Box<dyn std::error::Error>> {
        Ok(name
            .parse::<SegmentId>()
            .map_err(|error| error.to_string())?)
    }

    /// Hand over what may be handed over, and report it as kinds of things.
    fn fed(playout: &mut Playout) -> Vec<ToMedium> {
        let mut to_medium = Vec::new();
        playout.feed(&mut to_medium);
        to_medium
    }

    fn frames(to_medium: &[ToMedium]) -> usize {
        to_medium
            .iter()
            .filter(|item| matches!(item, ToMedium::Audio(_)))
            .count()
    }

    fn marks(to_medium: &[ToMedium]) -> Vec<u64> {
        to_medium
            .iter()
            .filter_map(|item| match item {
                ToMedium::Mark(id) => Some(*id),
                ToMedium::Audio(_)
                | ToMedium::Pause
                | ToMedium::AskStatus
                | ToMedium::Flush
                | ToMedium::Continue => None,
            })
            .collect()
    }

    #[test]
    fn a_segment_starts_at_its_first_mark_and_is_delivered_at_its_last() -> TestResult {
        let mut playout = Playout::default();
        playout.queue(segment("a")?)?;
        // Three frames and a half: the half is filled with silence.
        playout.audio(&segment("a")?, &vec![1; FRAME_BYTES * 3 + 320], true);
        let to_medium = fed(&mut playout);
        assert_eq!(frames(&to_medium), 4);
        assert!(matches!(to_medium.first(), Some(ToMedium::Mark(1))));
        assert!(matches!(to_medium.last(), Some(ToMedium::Mark(2))));
        assert!(
            matches!(to_medium.get(4), Some(ToMedium::Audio(frame)) if frame.ends_with(&[0; 320]) && frame.starts_with(&[1; 320]))
        );

        let mut outcomes = Vec::new();
        playout.mark_reached(1, &mut outcomes)?;
        assert_eq!(outcomes, [Outcome::Started(segment("a")?)]);
        outcomes.clear();
        playout.mark_reached(2, &mut outcomes)?;
        assert_eq!(outcomes, [Outcome::Delivered(segment("a")?)]);
        Ok(())
    }

    #[test]
    fn audio_short_of_a_frame_waits_for_the_rest_of_its_segment() -> TestResult {
        let mut playout = Playout::default();
        playout.queue(segment("a")?)?;
        playout.audio(&segment("a")?, &[1; 600], false);
        assert_eq!(fed(&mut playout), []);
        playout.audio(&segment("a")?, &[1; 40], false);
        let to_medium = fed(&mut playout);
        assert_eq!((frames(&to_medium), marks(&to_medium)), (1, vec![1]));
        Ok(())
    }

    #[test]
    fn a_segment_still_arriving_holds_back_the_ones_behind_it() -> TestResult {
        let mut playout = Playout::default();
        playout.queue(segment("a")?)?;
        playout.queue(segment("b")?)?;
        playout.audio(&segment("b")?, &[2; FRAME_BYTES], true);
        assert_eq!(fed(&mut playout), []);
        playout.audio(&segment("a")?, &[], true);
        // "a" is empty: its two marks, then "b" with its frame between its own.
        let to_medium = fed(&mut playout);
        assert_eq!(
            (frames(&to_medium), marks(&to_medium)),
            (1, vec![1, 2, 3, 4])
        );
        Ok(())
    }

    #[test]
    fn asterisk_is_given_a_window_and_more_only_as_marks_come_back() -> TestResult {
        let mut playout = Playout::default();
        playout.queue(segment("a")?)?;
        playout.audio(&segment("a")?, &vec![1; FRAME_BYTES * 400], true);
        let to_medium = fed(&mut playout);
        assert_eq!(frames(&to_medium), 250);
        assert_eq!(fed(&mut playout), []);

        // The start mark and the first ten frames are reported: ten more go.
        let mut outcomes = Vec::new();
        playout.mark_reached(1, &mut outcomes)?;
        playout.mark_reached(2, &mut outcomes)?;
        assert_eq!(frames(&fed(&mut playout)), 10);
        Ok(())
    }

    #[test]
    fn a_flush_reports_what_asterisk_itself_counted() -> TestResult {
        let mut playout = Playout::default();
        playout.queue(segment("a")?)?;
        playout.queue(segment("b")?)?;
        playout.audio(&segment("a")?, &vec![1; FRAME_BYTES * 100], true);
        playout.audio(&segment("b")?, &vec![2; FRAME_BYTES * 5], true);
        let to_medium = fed(&mut playout);
        assert_eq!(frames(&to_medium), 105);

        let mut outcomes = Vec::new();
        playout.mark_reached(1, &mut outcomes)?;
        playout.mark_reached(2, &mut outcomes)?;
        outcomes.clear();

        let mut to_medium = Vec::new();
        playout.flush(&mut to_medium, &mut outcomes);
        assert_eq!(to_medium, [ToMedium::Pause, ToMedium::AskStatus]);
        assert_eq!(outcomes, []);
        // A segment queued while the flush is under way is not part of it,
        // and nothing of it is handed over until the flush is done.
        playout.queue(segment("c")?)?;
        playout.audio(&segment("c")?, &[3; FRAME_BYTES], true);
        assert_eq!(fed(&mut playout), []);

        // Of 105 frames Asterisk still holds 88: it has played 17, ten of
        // them already reported by a mark.
        to_medium.clear();
        playout.status(88, &mut to_medium, &mut outcomes);
        assert_eq!(to_medium, [ToMedium::Flush, ToMedium::Continue]);
        assert_eq!(
            outcomes,
            [
                Outcome::Dropped {
                    segment: segment("a")?,
                    delivered_ms: 340
                },
                Outcome::Dropped {
                    segment: segment("b")?,
                    delivered_ms: 0
                },
            ]
        );
        assert_eq!(frames(&fed(&mut playout)), 1);
        Ok(())
    }

    #[test]
    fn a_segment_played_to_its_end_is_delivered_even_if_a_flush_caught_its_last_mark() -> TestResult
    {
        let mut playout = Playout::default();
        playout.queue(segment("a")?)?;
        playout.audio(&segment("a")?, &[1; FRAME_BYTES * 2], true);
        fed(&mut playout);
        let mut outcomes = Vec::new();
        playout.mark_reached(1, &mut outcomes)?;
        outcomes.clear();
        let mut to_medium = Vec::new();
        playout.flush(&mut to_medium, &mut outcomes);
        playout.status(0, &mut to_medium, &mut outcomes);
        assert_eq!(outcomes, [Outcome::Delivered(segment("a")?)]);
        Ok(())
    }

    #[test]
    fn a_flush_of_what_asterisk_never_got_needs_no_word_from_asterisk() -> TestResult {
        let mut playout = Playout::default();
        playout.queue(segment("a")?)?;
        let mut to_medium = Vec::new();
        let mut outcomes = Vec::new();
        playout.flush(&mut to_medium, &mut outcomes);
        assert_eq!(to_medium, []);
        assert_eq!(
            outcomes,
            [Outcome::Dropped {
                segment: segment("a")?,
                delivered_ms: 0
            }]
        );
        Ok(())
    }

    #[test]
    fn a_last_frame_filled_with_silence_still_makes_the_segment_whole() -> TestResult {
        let mut playout = Playout::default();
        playout.queue(segment("a")?)?;
        // Ten milliseconds of audio, handed over as one frame of twenty.
        playout.audio(&segment("a")?, &[1; 320], true);
        playout.queue(segment("b")?)?;
        fed(&mut playout);
        let mut outcomes = Vec::new();
        playout.mark_reached(1, &mut outcomes)?;
        outcomes.clear();
        let mut to_medium = Vec::new();
        playout.flush(&mut to_medium, &mut outcomes);
        // Asterisk holds nothing, but the end mark was not reported: the
        // frame is credited, the segment is whole.
        playout.status(0, &mut to_medium, &mut outcomes);
        assert_eq!(
            outcomes,
            [
                Outcome::Delivered(segment("a")?),
                Outcome::Dropped {
                    segment: segment("b")?,
                    delivered_ms: 0
                }
            ]
        );
        Ok(())
    }

    #[test]
    fn a_name_still_in_the_queue_is_refused_and_free_again_afterwards() -> TestResult {
        let mut playout = Playout::default();
        playout.queue(segment("a")?)?;
        assert_eq!(playout.queue(segment("a")?), Err(AlreadyQueued));
        let mut outcomes = Vec::new();
        playout.abandon(&mut outcomes);
        assert_eq!(
            outcomes,
            [Outcome::Dropped {
                segment: segment("a")?,
                delivered_ms: 0
            }]
        );
        assert_eq!(playout.queue(segment("a")?), Ok(()));
        Ok(())
    }

    #[test]
    fn audio_for_a_segment_nobody_queued_is_left_out() -> TestResult {
        let mut playout = Playout::default();
        playout.audio(&segment("ghost")?, &[1; FRAME_BYTES], true);
        assert_eq!(fed(&mut playout), []);
        Ok(())
    }

    #[test]
    fn a_mark_out_of_order_is_an_error() -> TestResult {
        let mut playout = Playout::default();
        playout.queue(segment("a")?)?;
        playout.audio(&segment("a")?, &[1; FRAME_BYTES], true);
        fed(&mut playout);
        let mut outcomes = Vec::new();
        assert_eq!(
            playout.mark_reached(2, &mut outcomes),
            Err(UnexpectedMark(2))
        );
        Ok(())
    }
}

//! Queue tests driven through the public [`VirtQueue`] surface, with the
//! device side played by writing into the rings the queue programmed.

use super::packed::PackedRing;
use super::split::SplitRing;
use super::{ChainEntry, MAX_CHAIN_BUFFERS, Ring, VirtQueue, VirtqueueError, build_chain};
use helios_hal::dma::DmaBuffer;

use crate::features::NegotiatedFeatures;
use crate::testing::{FakeTransport, FakeTransportConfig, WindowDmaPool};
use crate::transport::VirtioFeatures;
use helios_hal::io::IoError;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const DESC_F_INDIRECT: u16 = 4;
const PACKED_F_AVAIL: u16 = 1 << 7;
const PACKED_F_USED: u16 = 1 << 15;
const USED_FLAG_NO_NOTIFY: u16 = 1;
const PACKED_EVENT_FLAG_DESC: u16 = 2;
const PACKED_EVENT_FLAG_DISABLE: u16 = 1;

fn transport() -> FakeTransport {
    FakeTransport::new(FakeTransportConfig::default())
}

fn features(extra: VirtioFeatures) -> NegotiatedFeatures {
    NegotiatedFeatures::from_bits(VirtioFeatures::VERSION_1.bits() | extra.bits())
}

fn split(queue: &VirtQueue<FakeTransport>) -> &SplitRing<FakeTransport> {
    match &queue.ring {
        Ring::Split(ring) => ring,
        Ring::Packed(_) => panic!("expected a split ring"),
    }
}

fn packed(queue: &VirtQueue<FakeTransport>) -> &PackedRing<FakeTransport> {
    match &queue.ring {
        Ring::Packed(ring) => ring,
        Ring::Split(_) => panic!("expected a packed ring"),
    }
}

#[test]
fn split_submission_links_a_chain_and_completion_recycles_it() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 2, features(VirtioFeatures::empty()))
        .expect("queue should initialize");

    let input = [1_u8; 16];
    let mut output = [0_u8; 16];
    let token = queue
        .submit(&transport, &[&input], &mut [&mut output])
        .expect("submission should succeed");
    assert_eq!(token, 0);

    let head = split(&queue).descriptor(0);
    assert_eq!(head.addr, input.as_ptr() as usize as u64);
    assert_eq!(head.flags & DESC_F_NEXT, DESC_F_NEXT);
    let tail = split(&queue).descriptor(head.next);
    assert_eq!(tail.addr, output.as_mut_ptr() as usize as u64);
    assert_eq!(tail.flags & DESC_F_WRITE, DESC_F_WRITE);
    assert_eq!(tail.flags & DESC_F_NEXT, 0);
    assert_eq!(split(&queue).published_avail_idx(), 1);
    assert_eq!(queue.available_descriptors(), 6);

    split(&queue).device_complete(token, 16);
    assert_eq!(queue.pop_used_with_len(), Ok(Some((token, 16))));
    assert_eq!(
        queue.available_descriptors(),
        8,
        "a completion returns the whole chain"
    );
}

#[test]
fn split_deferred_submissions_become_visible_only_on_publication() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::empty()))
        .expect("queue should initialize");

    let first = [1_u8; 16];
    let second = [2_u8; 16];
    queue
        .submit_read_only_deferred(&transport, &first)
        .expect("first deferred submission should succeed");
    queue
        .submit_read_only_deferred(&transport, &second)
        .expect("second deferred submission should succeed");
    assert_eq!(split(&queue).published_avail_idx(), 0);

    queue.publish();
    assert_eq!(split(&queue).published_avail_idx(), 2);
}

#[test]
fn split_indirect_chains_live_in_the_head_table() {
    let transport = transport();
    let mut queue = VirtQueue::new(
        &transport,
        0,
        8,
        2,
        features(VirtioFeatures::RING_INDIRECT_DESC),
    )
    .expect("queue should initialize");

    let input = [7_u8; 24];
    let mut output = [0_u8; 32];
    let token = queue
        .submit(&transport, &[&input], &mut [&mut output])
        .expect("submission should succeed");

    let head = split(&queue).descriptor(token);
    assert_eq!(head.flags, DESC_F_INDIRECT);
    assert_eq!(head.len, 32, "two 16-byte indirect descriptors");
    assert_eq!(
        queue.available_descriptors(),
        7,
        "an indirect chain costs one ring descriptor"
    );

    let first = split(&queue).indirect_descriptor(token, 0);
    assert_eq!(first.addr, input.as_ptr() as usize as u64);
    assert_eq!(first.len, 24);
    assert_eq!(first.flags, DESC_F_NEXT);
    assert_eq!(first.next, 1);
    let second = split(&queue).indirect_descriptor(token, 1);
    assert_eq!(second.addr, output.as_mut_ptr() as usize as u64);
    assert_eq!(second.flags, DESC_F_WRITE);

    split(&queue).device_complete(token, 32);
    assert_eq!(queue.pop_used_with_len(), Ok(Some((token, 32))));
    assert_eq!(queue.available_descriptors(), 8);
}

#[test]
fn split_completions_are_delivered_out_of_order() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::empty()))
        .expect("queue should initialize");

    let first = [1_u8; 8];
    let second = [2_u8; 8];
    let first_token = queue
        .submit(&transport, &[&first], &mut [])
        .expect("first submission should succeed");
    let second_token = queue
        .submit(&transport, &[&second], &mut [])
        .expect("second submission should succeed");

    // The device finishes the younger request first.
    split(&queue).device_complete(second_token, 8);
    split(&queue).device_complete(first_token, 4);

    assert_eq!(queue.pop_used_with_len(), Ok(Some((second_token, 8))));
    assert_eq!(queue.pop_used_with_len(), Ok(Some((first_token, 4))));
    assert_eq!(queue.pop_used(), Ok(None));
}

#[test]
fn split_publishes_the_used_event_as_completions_are_consumed() {
    let transport = transport();
    let mut queue = VirtQueue::new(
        &transport,
        0,
        8,
        1,
        features(VirtioFeatures::RING_EVENT_IDX),
    )
    .expect("queue should initialize");

    let buffer = [1_u8; 8];
    let token = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");
    assert_eq!(split(&queue).published_used_event(), 0);

    split(&queue).device_complete(token, 0);
    queue
        .pop_used()
        .expect("the used ring is intact")
        .expect("completion should be visible");
    assert_eq!(
        split(&queue).published_used_event(),
        1,
        "the driver tells the device how far it has consumed"
    );
}

#[test]
fn split_event_index_suppresses_a_kick_outside_the_device_window() {
    let transport = transport();
    let mut queue = VirtQueue::new(
        &transport,
        0,
        8,
        1,
        features(VirtioFeatures::RING_EVENT_IDX),
    )
    .expect("queue should initialize");

    // The device asks for a kick once entry 0 is published.
    split(&queue).device_set_avail_event(0);
    let first = [1_u8; 8];
    queue
        .submit(&transport, &[&first], &mut [])
        .expect("submission should succeed");
    queue.notify(&transport);
    assert_eq!(transport.kick_count(), 1);

    // The device is still working through entry 0, so publishing entry 1
    // must not kick.
    split(&queue).device_set_avail_event(0);
    let second = [2_u8; 8];
    queue
        .submit(&transport, &[&second], &mut [])
        .expect("submission should succeed");
    queue.notify(&transport);
    assert_eq!(transport.kick_count(), 1, "the kick must stay suppressed");
}

#[test]
fn split_no_notify_flag_suppresses_the_kick() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::empty()))
        .expect("queue should initialize");

    let buffer = [1_u8; 8];
    queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");
    queue.notify(&transport);
    assert_eq!(transport.kick_count(), 1);

    split(&queue).device_set_used_flags(USED_FLAG_NO_NOTIFY);
    queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");
    queue.notify(&transport);
    assert_eq!(transport.kick_count(), 1);
}

#[test]
fn split_available_index_keeps_working_past_a_u16_wrap() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::empty()))
        .expect("queue should initialize");

    let buffer = [3_u8; 8];
    let rounds = u32::from(u16::MAX) + 5;
    for round in 0..rounds {
        let token = queue
            .submit(&transport, &[&buffer], &mut [])
            .unwrap_or_else(|error| panic!("submission {round} should succeed: {error:?}"));
        split(&queue).device_complete(token, 8);
        assert_eq!(queue.pop_used_with_len(), Ok(Some((token, 8))));
    }

    assert_eq!(
        split(&queue).published_avail_idx(),
        (rounds % (u32::from(u16::MAX) + 1)) as u16
    );
    assert_eq!(queue.available_descriptors(), 8);
}

#[test]
fn chains_longer_than_the_queue_limit_are_rejected() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 2, features(VirtioFeatures::empty()))
        .expect("queue should initialize");

    let buffer = [1_u8; 8];
    queue
        .submit_read_only_chain_deferred(&transport, &[&buffer, &buffer, &buffer])
        .expect_err("a three-buffer chain exceeds the queue's limit of two");
    assert_eq!(
        queue.available_descriptors(),
        8,
        "a rejected submission must not consume descriptors"
    );
    assert_eq!(queue.next_free_descriptor(), 0);
}

#[test]
fn a_full_ring_rejects_a_chain_without_disturbing_the_pool() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 2, features(VirtioFeatures::empty()))
        .expect("queue should initialize");

    let buffer = [1_u8; 8];
    for _ in 0..7 {
        queue
            .submit_read_only_deferred(&transport, &buffer)
            .expect("the ring has room");
    }
    assert_eq!(queue.available_descriptors(), 1);
    let head = queue.next_free_descriptor();

    queue
        .submit_read_only_chain_deferred(&transport, &[&buffer, &buffer])
        .expect_err("a two-descriptor chain does not fit in one free descriptor");

    assert_eq!(queue.available_descriptors(), 1);
    assert_eq!(queue.next_free_descriptor(), head);
    queue
        .submit_read_only_deferred(&transport, &buffer)
        .expect("the last descriptor is still usable");
}

#[test]
fn split_notification_data_carries_the_available_index() {
    let transport = transport();
    let mut queue = VirtQueue::new(
        &transport,
        3,
        8,
        1,
        features(VirtioFeatures::NOTIFICATION_DATA),
    )
    .expect("queue should initialize");

    let buffer = [1_u8; 8];
    queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");
    queue.notify(&transport);

    assert_eq!(
        transport.notification_data(),
        alloc::vec![3 | (1 << 16)],
        "the payload is the queue index plus the next available index"
    );
}

#[test]
fn split_in_order_batch_completion_expands_to_every_chain() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::IN_ORDER))
        .expect("queue should initialize");

    let buffer = [1_u8; 8];
    let mut output = [0_u8; 64];
    let first = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("first submission should succeed");
    let second = queue
        .submit(&transport, &[], &mut [&mut output])
        .expect("second submission should succeed");
    let third = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("third submission should succeed");

    // The device reports all three with the single used entry the
    // feature allows, naming the last chain of the batch.
    split(&queue).device_complete_batch(third, 8, 3);

    assert_eq!(
        queue.pop_used_with_len(),
        Ok(Some((first, 0))),
        "a read-only chain the device skipped over wrote nothing"
    );
    assert_eq!(
        queue.pop_used_with_len(),
        Ok(Some((second, 64))),
        "a skipped writable chain counts as used completely"
    );
    assert_eq!(
        queue.pop_used_with_len(),
        Ok(Some((third, 8))),
        "the entry the device wrote carries the real length"
    );
    assert_eq!(queue.pop_used(), Ok(None));
    assert_eq!(queue.available_descriptors(), 8);
}

#[test]
fn split_in_order_single_completions_use_the_reported_length() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::IN_ORDER))
        .expect("queue should initialize");

    let mut first = [0_u8; 32];
    let mut second = [0_u8; 32];
    let first_token = queue
        .submit(&transport, &[], &mut [&mut first])
        .expect("first submission should succeed");
    let second_token = queue
        .submit(&transport, &[], &mut [&mut second])
        .expect("second submission should succeed");

    split(&queue).device_complete(first_token, 5);
    split(&queue).device_complete(second_token, 9);

    assert_eq!(queue.pop_used_with_len(), Ok(Some((first_token, 5))));
    assert_eq!(queue.pop_used_with_len(), Ok(Some((second_token, 9))));
}

#[test]
fn packed_submission_marks_the_head_available_last() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 2, features(VirtioFeatures::RING_PACKED))
        .expect("queue should initialize");

    let input = [1_u8; 16];
    let mut output = [0_u8; 16];
    let id = queue
        .submit(&transport, &[&input], &mut [&mut output])
        .expect("submission should succeed");
    assert_eq!(id, 0);

    let head = packed(&queue).descriptor(0);
    assert_eq!(head.addr, input.as_ptr() as usize as u64);
    assert_eq!(head.id, id);
    assert_eq!(head.flags & PACKED_F_AVAIL, PACKED_F_AVAIL);
    assert_eq!(head.flags & PACKED_F_USED, 0);
    assert_eq!(head.flags & DESC_F_NEXT, DESC_F_NEXT);
    let tail = packed(&queue).descriptor(1);
    assert_eq!(tail.addr, output.as_mut_ptr() as usize as u64);
    assert_eq!(tail.id, id);
    assert_eq!(tail.flags & DESC_F_WRITE, DESC_F_WRITE);
    assert_eq!(tail.flags & DESC_F_NEXT, 0);
    assert_eq!(packed(&queue).avail_position(), (2, true));
    assert_eq!(queue.available_descriptors(), 6);

    packed(&queue).device_complete(0, true, id, 16);
    assert_eq!(queue.pop_used_with_len(), Ok(Some((id, 16))));
    assert_eq!(
        queue.available_descriptors(),
        8,
        "a completion returns both ring positions"
    );
}

#[test]
fn packed_wrap_counter_flips_when_the_ring_turns_over() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 4, 1, features(VirtioFeatures::RING_PACKED))
        .expect("queue should initialize");

    let buffer = [1_u8; 8];
    let mut ids = alloc::vec::Vec::new();
    for _ in 0..4 {
        ids.push(
            queue
                .submit(&transport, &[&buffer], &mut [])
                .expect("the ring has room"),
        );
    }
    assert_eq!(
        packed(&queue).avail_position(),
        (0, false),
        "a full lap flips the driver's wrap counter"
    );
    for position in 0..4 {
        let descriptor = packed(&queue).descriptor(position);
        assert_eq!(descriptor.flags & PACKED_F_AVAIL, PACKED_F_AVAIL);
        assert_eq!(descriptor.flags & PACKED_F_USED, 0);
    }

    for (position, id) in ids.iter().copied().enumerate() {
        packed(&queue).device_complete(position as u16, true, id, 8);
        assert_eq!(queue.pop_used_with_len(), Ok(Some((id, 8))));
    }

    // The second lap writes the opposite availability pattern.
    let id = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("the ring is empty again");
    let descriptor = packed(&queue).descriptor(0);
    assert_eq!(descriptor.flags & PACKED_F_AVAIL, 0);
    assert_eq!(descriptor.flags & PACKED_F_USED, PACKED_F_USED);
    packed(&queue).device_complete(0, false, id, 8);
    assert_eq!(queue.pop_used_with_len(), Ok(Some((id, 8))));
}

#[test]
fn packed_indirect_chains_live_in_the_head_table() {
    let transport = transport();
    let mut queue = VirtQueue::new(
        &transport,
        0,
        8,
        2,
        features(VirtioFeatures::RING_PACKED | VirtioFeatures::RING_INDIRECT_DESC),
    )
    .expect("queue should initialize");

    let input = [7_u8; 24];
    let mut output = [0_u8; 32];
    let id = queue
        .submit(&transport, &[&input], &mut [&mut output])
        .expect("submission should succeed");

    let head = packed(&queue).descriptor(0);
    assert_eq!(head.flags & DESC_F_INDIRECT, DESC_F_INDIRECT);
    assert_eq!(head.flags & PACKED_F_AVAIL, PACKED_F_AVAIL);
    assert_eq!(head.len, 32);
    assert_eq!(head.id, id);
    assert_eq!(
        packed(&queue).avail_position(),
        (1, true),
        "an indirect chain costs one ring position"
    );

    let first = packed(&queue).indirect_descriptor(id, 0);
    assert_eq!(first.addr, input.as_ptr() as usize as u64);
    assert_eq!(first.len, 24);
    assert_eq!(first.flags, 0, "read-only, and packed tables do not chain");
    let second = packed(&queue).indirect_descriptor(id, 1);
    assert_eq!(second.addr, output.as_mut_ptr() as usize as u64);
    assert_eq!(second.flags, DESC_F_WRITE);
}

#[test]
fn packed_completions_are_delivered_out_of_order() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::RING_PACKED))
        .expect("queue should initialize");

    let buffer = [1_u8; 8];
    let first = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("first submission should succeed");
    let second = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("second submission should succeed");

    // The device writes used descriptors from its own cursor, naming the
    // younger request first.
    packed(&queue).device_complete(0, true, second, 3);
    packed(&queue).device_complete(1, true, first, 5);

    assert_eq!(queue.pop_used_with_len(), Ok(Some((second, 3))));
    assert_eq!(queue.pop_used_with_len(), Ok(Some((first, 5))));
    assert_eq!(queue.pop_used(), Ok(None));
}

#[test]
fn packed_in_order_batch_completion_expands_to_every_chain() {
    let transport = transport();
    let mut queue = VirtQueue::new(
        &transport,
        0,
        8,
        1,
        features(VirtioFeatures::RING_PACKED | VirtioFeatures::IN_ORDER),
    )
    .expect("queue should initialize");

    let buffer = [1_u8; 8];
    let mut output = [0_u8; 48];
    let first = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("first submission should succeed");
    let second = queue
        .submit(&transport, &[], &mut [&mut output])
        .expect("second submission should succeed");
    let third = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("third submission should succeed");

    // One used descriptor at the position of the batch's first
    // descriptor, naming the batch's last buffer id.
    packed(&queue).device_complete(0, true, third, 8);

    assert_eq!(queue.pop_used_with_len(), Ok(Some((first, 0))));
    assert_eq!(queue.pop_used_with_len(), Ok(Some((second, 48))));
    assert_eq!(queue.pop_used_with_len(), Ok(Some((third, 8))));
    assert_eq!(queue.pop_used(), Ok(None));
    assert_eq!(queue.available_descriptors(), 8);
}

#[test]
fn packed_event_suppression_drives_the_kick_decision() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::RING_PACKED))
        .expect("queue should initialize");

    let buffer = [1_u8; 8];
    queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");
    queue.notify(&transport);
    assert_eq!(transport.kick_count(), 1, "an enabled device is kicked");

    packed(&queue).device_set_event(0, PACKED_EVENT_FLAG_DISABLE);
    queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");
    queue.notify(&transport);
    assert_eq!(transport.kick_count(), 1, "a disabled device is not kicked");

    // Descriptor-granular suppression: the device wants a kick when ring
    // position 2 becomes available, which the next submission publishes.
    packed(&queue).device_set_event(2 | (1 << 15), PACKED_EVENT_FLAG_DESC);
    queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");
    queue.notify(&transport);
    assert_eq!(transport.kick_count(), 2);
}

#[test]
fn packed_publishes_used_event_with_its_wrap_counter() {
    let transport = transport();
    let mut queue = VirtQueue::new(
        &transport,
        0,
        4,
        1,
        features(VirtioFeatures::RING_PACKED | VirtioFeatures::RING_EVENT_IDX),
    )
    .expect("queue should initialize");

    assert_eq!(
        packed(&queue).driver_event(),
        (1 << 15, PACKED_EVENT_FLAG_DESC),
        "the driver starts with wrap counter one and descriptor granularity"
    );

    let buffer = [1_u8; 8];
    let id = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");
    packed(&queue).device_complete(0, true, id, 8);
    queue
        .pop_used()
        .expect("the used ring is intact")
        .expect("completion should be visible");

    assert_eq!(
        packed(&queue).driver_event(),
        (1 | (1 << 15), PACKED_EVENT_FLAG_DESC)
    );
}

#[test]
fn packed_notification_data_carries_the_ring_position_and_wrap() {
    let transport = transport();
    let mut queue = VirtQueue::new(
        &transport,
        5,
        4,
        1,
        features(VirtioFeatures::RING_PACKED | VirtioFeatures::NOTIFICATION_DATA),
    )
    .expect("queue should initialize");

    let buffer = [1_u8; 8];
    for _ in 0..4 {
        queue
            .submit(&transport, &[&buffer], &mut [])
            .expect("the ring has room");
    }
    queue.notify(&transport);

    assert_eq!(
        transport.notification_data(),
        alloc::vec![5],
        "after a full lap the position is zero and the wrap counter cleared"
    );
}

#[test]
fn queue_reset_recycles_in_flight_descriptors_and_reprograms_the_ring() {
    for extra in [VirtioFeatures::empty(), VirtioFeatures::RING_PACKED] {
        let transport = transport();
        let mut queue = VirtQueue::new(
            &transport,
            2,
            8,
            1,
            features(VirtioFeatures::RING_RESET | extra),
        )
        .expect("queue should initialize");

        let buffer = [1_u8; 8];
        for _ in 0..3 {
            queue
                .submit(&transport, &[&buffer], &mut [])
                .expect("the ring has room");
        }
        assert_eq!(queue.available_descriptors(), 5);

        queue.reset(&transport).expect("reset should succeed");

        assert_eq!(transport.queue_resets(), alloc::vec![2]);
        assert_eq!(
            queue.available_descriptors(),
            8,
            "abandoned chains return to the pool"
        );
        assert_eq!(queue.next_free_descriptor(), 0);
        let programmed = transport.programmed_queues();
        assert_eq!(programmed.len(), 2, "the queue is programmed again");
        assert_eq!(programmed[0], programmed[1]);

        let token = queue
            .submit(&transport, &[&buffer], &mut [])
            .expect("the queue works after a reset");
        assert_eq!(token, 0);
    }
}

#[test]
fn a_queue_without_the_reset_feature_refuses_to_reset() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::empty()))
        .expect("queue should initialize");

    queue
        .reset(&transport)
        .expect_err("VIRTIO_F_RING_RESET was not negotiated");
    assert!(transport.queue_resets().is_empty());
}

/// A device that completes an identifier the driver does not own must
/// fault the queue without touching the identifier pool, and the queue
/// must then refuse further work until it is reset.
fn assert_faulted_without_touching_the_pool(
    transport: &FakeTransport,
    queue: &mut VirtQueue<FakeTransport>,
    free_before: usize,
) {
    assert_eq!(queue.pop_used_with_len(), Err(IoError::DeviceFault));
    assert!(queue.is_faulted());
    assert_eq!(
        queue.available_descriptors(),
        free_before,
        "a rejected completion must not release anything"
    );
    let buffer = [1_u8; 8];
    assert_eq!(
        queue.submit(transport, &[&buffer], &mut []),
        Err(IoError::DeviceFault),
        "a faulted queue is not driven further"
    );
    assert_eq!(queue.pop_used_with_len(), Err(IoError::DeviceFault));
}

#[test]
fn split_a_repeated_used_id_is_a_device_fault() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 2, features(VirtioFeatures::empty()))
        .expect("queue should initialize");
    let input = [1_u8; 16];
    let mut output = [0_u8; 16];
    let token = queue
        .submit(&transport, &[&input], &mut [&mut output])
        .expect("submission should succeed");

    split(&queue).device_complete(token, 16);
    assert_eq!(queue.pop_used_with_len(), Ok(Some((token, 16))));
    assert_eq!(queue.available_descriptors(), 8);

    split(&queue).device_complete(token, 16);
    assert_faulted_without_touching_the_pool(&transport, &mut queue, 8);
}

#[test]
fn split_an_unsubmitted_used_id_is_a_device_fault() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 2, features(VirtioFeatures::empty()))
        .expect("queue should initialize");
    let input = [1_u8; 16];
    let mut output = [0_u8; 16];
    queue
        .submit(&transport, &[&input], &mut [&mut output])
        .expect("submission should succeed");
    let in_flight = queue.available_descriptors();

    // Descriptor 1 is the tail of the chain in flight, not a chain head.
    split(&queue).device_complete(1, 16);
    assert_faulted_without_touching_the_pool(&transport, &mut queue, in_flight);
}

#[test]
fn split_an_out_of_range_used_id_is_a_device_fault() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::empty()))
        .expect("queue should initialize");
    let buffer = [1_u8; 8];
    queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");
    let in_flight = queue.available_descriptors();

    // 0x1_0000 truncates to descriptor 0, which is in flight: the id
    // is judged at the width the device wrote it.
    split(&queue).device_complete_raw(0x1_0000, 8);
    assert_faulted_without_touching_the_pool(&transport, &mut queue, in_flight);
}

#[test]
fn split_an_id_past_the_ring_is_a_device_fault() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::empty()))
        .expect("queue should initialize");

    split(&queue).device_complete_raw(8, 0);
    assert_faulted_without_touching_the_pool(&transport, &mut queue, 8);
}

#[test]
fn split_in_order_a_completion_naming_an_unsubmitted_id_is_a_device_fault() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::IN_ORDER))
        .expect("queue should initialize");
    let buffer = [1_u8; 8];
    for _ in 0..2 {
        queue
            .submit(&transport, &[&buffer], &mut [])
            .expect("the ring has room");
    }
    let in_flight = queue.available_descriptors();

    split(&queue).device_complete(6, 8);
    assert_faulted_without_touching_the_pool(&transport, &mut queue, in_flight);
}

#[test]
fn split_in_order_a_completion_with_nothing_in_flight_is_a_device_fault() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::IN_ORDER))
        .expect("queue should initialize");

    split(&queue).device_complete(0, 8);
    assert_faulted_without_touching_the_pool(&transport, &mut queue, 8);
}

#[test]
fn packed_a_repeated_used_id_is_a_device_fault() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::RING_PACKED))
        .expect("queue should initialize");
    let buffer = [1_u8; 8];
    let id = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");

    packed(&queue).device_complete(0, true, id, 8);
    assert_eq!(queue.pop_used_with_len(), Ok(Some((id, 8))));
    assert_eq!(queue.available_descriptors(), 8);

    packed(&queue).device_complete(1, true, id, 8);
    assert_faulted_without_touching_the_pool(&transport, &mut queue, 8);
}

#[test]
fn packed_an_unsubmitted_or_out_of_range_used_id_is_a_device_fault() {
    for bogus in [5_u16, 8, u16::MAX] {
        let transport = transport();
        let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::RING_PACKED))
            .expect("queue should initialize");
        let buffer = [1_u8; 8];
        queue
            .submit(&transport, &[&buffer], &mut [])
            .expect("submission should succeed");
        let in_flight = queue.available_descriptors();

        packed(&queue).device_complete(0, true, bogus, 8);
        assert_faulted_without_touching_the_pool(&transport, &mut queue, in_flight);
    }
}

#[test]
fn packed_in_order_a_batch_naming_an_unsubmitted_id_is_a_device_fault() {
    let transport = transport();
    let mut queue = VirtQueue::new(
        &transport,
        0,
        8,
        1,
        features(VirtioFeatures::RING_PACKED | VirtioFeatures::IN_ORDER),
    )
    .expect("queue should initialize");
    let buffer = [1_u8; 8];
    queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("submission should succeed");
    let in_flight = queue.available_descriptors();

    packed(&queue).device_complete(0, true, 6, 8);
    assert_faulted_without_touching_the_pool(&transport, &mut queue, in_flight);
}

#[test]
fn a_reset_lets_a_faulted_queue_run_again() {
    let transport = transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 1, features(VirtioFeatures::RING_RESET))
        .expect("queue should initialize");
    split(&queue).device_complete_raw(8, 0);
    assert_eq!(queue.pop_used(), Err(IoError::DeviceFault));

    queue.reset(&transport).expect("the queue can be reset");
    assert!(!queue.is_faulted());
    let buffer = [1_u8; 8];
    let token = queue
        .submit(&transport, &[&buffer], &mut [])
        .expect("the queue works after a reset");
    split(&queue).device_complete(token, 8);
    assert_eq!(queue.pop_used_with_len(), Ok(Some((token, 8))));
}

/// A transport whose pool translates a 64 KiB window and is followed by
/// a page of real memory the device cannot reach.
fn windowed_transport() -> (WindowDmaPool, FakeTransport<WindowDmaPool>) {
    let pool = WindowDmaPool::with_unmapped_tail(16 * 4096, 4096);
    let transport = FakeTransport::with_dma(FakeTransportConfig::default(), pool.clone());
    (pool, transport)
}

fn empty_chain() -> [ChainEntry; MAX_CHAIN_BUFFERS] {
    [ChainEntry {
        addr: 0,
        len: 0,
        writable: false,
    }; MAX_CHAIN_BUFFERS]
}

#[test]
fn a_buffer_running_past_the_window_is_refused_with_its_address() {
    let (pool, transport) = windowed_transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 2, features(VirtioFeatures::empty()))
        .expect("queue should initialize");
    // Starts inside the window, so a first-byte translation accepts it.
    let straddling = pool.straddling_the_window_end(16, 16);
    let refused = VirtqueueError::Unreachable {
        address: straddling.as_ptr() as usize,
        bytes: 32,
    };

    let mut chain = empty_chain();
    assert_eq!(
        build_chain(&transport, &[straddling], None, &mut [], &mut chain),
        Err(refused)
    );

    let in_flight = queue.available_descriptors();
    assert_eq!(
        queue.submit(&transport, &[straddling], &mut []),
        Err(IoError::DeviceFault)
    );
    assert_eq!(queue.available_descriptors(), in_flight);
}

#[test]
fn a_buffer_wholly_inside_the_window_translates_as_before() {
    let (pool, transport) = windowed_transport();
    let mut queue = VirtQueue::new(&transport, 0, 8, 2, features(VirtioFeatures::empty()))
        .expect("queue should initialize");
    let page = pool.pages(1);
    let bytes = unsafe { core::slice::from_raw_parts(page.as_ptr(), 4096) };

    let mut chain = empty_chain();
    let used = build_chain(&transport, &[bytes], None, &mut [], &mut chain)
        .expect("a page inside the window translates");
    assert_eq!(used, 1);
    assert_eq!(chain[0].addr, page.device_address().get());
    assert_eq!(chain[0].len, 4096);
    assert!(!chain[0].writable);

    queue
        .submit(&transport, &[bytes], &mut [])
        .expect("the same page can be submitted");
}

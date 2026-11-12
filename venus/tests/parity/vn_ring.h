/*
 * The ring the generated `vn_call_*` wrappers submit commands to. The parity
 * run calls the encoders directly and never submits, so the wrappers are
 * compiled but unreachable.
 */

#ifndef VN_RING_H
#define VN_RING_H

#include "vn_cs.h"

#define VN_TRACE_FUNC()

struct vn_ring;

struct vn_ring_submit_command {
   int unused;
};

static inline struct vn_cs_encoder *
vn_ring_submit_command_init(struct vn_ring *ring,
                            struct vn_ring_submit_command *submit,
                            void *cmd_data,
                            size_t cmd_size,
                            size_t reply_size)
{
   parity_unreachable("vn_ring_submit_command_init");
   return NULL;
}

static inline void
vn_ring_submit_command(struct vn_ring *ring, struct vn_ring_submit_command *submit)
{
   parity_unreachable("vn_ring_submit_command");
}

static inline struct vn_cs_decoder *
vn_ring_get_command_reply(struct vn_ring *ring, struct vn_ring_submit_command *submit)
{
   parity_unreachable("vn_ring_get_command_reply");
   return NULL;
}

static inline void
vn_ring_free_command_reply(struct vn_ring *ring, struct vn_ring_submit_command *submit)
{
   parity_unreachable("vn_ring_free_command_reply");
}

#endif /* VN_RING_H */

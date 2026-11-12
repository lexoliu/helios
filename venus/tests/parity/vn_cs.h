/* The driver-side command stream the generated `vn_protocol_driver_*.h` expect. */

#ifndef VN_CS_H
#define VN_CS_H

#include "cs_core.h"

typedef uint64_t vn_object_id;

struct vn_cs_encoder {
   struct parity_encoder base;
};

struct vn_cs_decoder {
   struct parity_decoder base;
};

/* The renderer protocol the driver encodes for: an API version and a set of
 * extension numbers, set by the harness before each run. */
extern uint32_t parity_api_version;
extern const uint8_t *parity_extension_enabled;
extern uint32_t parity_extension_count;

static inline bool
vn_cs_renderer_protocol_has_api_version(uint32_t api_version)
{
   return parity_api_version >= api_version;
}

static inline bool
vn_cs_renderer_protocol_has_extension(uint32_t ext_number)
{
   return ext_number < parity_extension_count && parity_extension_enabled[ext_number];
}

static inline size_t
vn_cs_encoder_get_len(const struct vn_cs_encoder *enc)
{
   return enc->base.len;
}

static inline bool
vn_cs_encoder_reserve(struct vn_cs_encoder *enc, size_t size)
{
   return enc->base.len + size <= enc->base.capacity;
}

static inline void
vn_cs_encoder_write(struct vn_cs_encoder *enc, size_t size, const void *val, size_t val_size)
{
   parity_encoder_write(&enc->base, size, val, val_size);
}

static inline void
vn_cs_decoder_set_fatal(struct vn_cs_decoder *dec)
{
   dec->base.fatal = true;
}

static inline void
vn_cs_decoder_read(struct vn_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   parity_decoder_read(&dec->base, size, val, val_size);
}

static inline void
vn_cs_decoder_peek(const struct vn_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   parity_decoder_peek(&dec->base, size, val, val_size);
}

static inline vn_object_id
vn_cs_handle_load_id(const void **handle, VkObjectType type)
{
   return parity_handle_load_id(handle, type);
}

static inline void
vn_cs_handle_store_id(void **handle, vn_object_id id, VkObjectType type)
{
   parity_handle_store_id(handle, id, type);
}

#endif /* VN_CS_H */

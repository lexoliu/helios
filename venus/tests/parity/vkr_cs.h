/* The renderer-side command stream the generated `vn_protocol_renderer_*.h` expect. */

#ifndef VKR_CS_H
#define VKR_CS_H

#include "cs_core.h"

typedef uint64_t vkr_object_id;

/* What `vn_cs_get_object_handle` reads: a renderer object. The parity run
 * models an object by its id. */
struct vkr_object {
   struct {
      uint64_t u64;
   } handle;
};

struct vkr_cs_encoder {
   struct parity_encoder base;
};

struct vkr_cs_decoder {
   struct parity_decoder base;
};

static inline bool
vkr_cs_encoder_acquire(struct vkr_cs_encoder *enc)
{
   return true;
}

static inline void
vkr_cs_encoder_release(struct vkr_cs_encoder *enc)
{
}

static inline void
vkr_cs_encoder_write(struct vkr_cs_encoder *enc, size_t size, const void *val, size_t val_size)
{
   parity_encoder_write(&enc->base, size, val, val_size);
}

static inline void
vkr_cs_decoder_set_fatal(const struct vkr_cs_decoder *dec)
{
   ((struct vkr_cs_decoder *)dec)->base.fatal = true;
}

static inline bool
vkr_cs_decoder_get_fatal(const struct vkr_cs_decoder *dec)
{
   return dec->base.fatal;
}

static inline void
vkr_cs_decoder_read(struct vkr_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   parity_decoder_read(&dec->base, size, val, val_size);
}

static inline void
vkr_cs_decoder_peek(const struct vkr_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   parity_decoder_peek(&dec->base, size, val, val_size);
}

static inline struct vkr_object *
vkr_cs_decoder_lookup_object(const struct vkr_cs_decoder *dec, vkr_object_id id, VkObjectType type)
{
   parity_unreachable("vkr_cs_decoder_lookup_object");
   return NULL;
}

static inline void
vkr_cs_decoder_reset_temp_pool(struct vkr_cs_decoder *dec)
{
   parity_unreachable("vkr_cs_decoder_reset_temp_pool");
}

static inline void *
vkr_cs_decoder_alloc_temp(struct vkr_cs_decoder *dec, size_t size)
{
   parity_unreachable("vkr_cs_decoder_alloc_temp");
   return NULL;
}

static inline void *
vkr_cs_decoder_alloc_temp_array(struct vkr_cs_decoder *dec, size_t size, size_t count)
{
   parity_unreachable("vkr_cs_decoder_alloc_temp_array");
   return NULL;
}

static inline void *
vkr_cs_decoder_get_blob_storage(struct vkr_cs_decoder *dec, size_t size)
{
   parity_unreachable("vkr_cs_decoder_get_blob_storage");
   return NULL;
}

static inline void *
vkr_cs_encoder_get_blob_storage(struct vkr_cs_encoder *enc, size_t offset, size_t size)
{
   parity_unreachable("vkr_cs_encoder_get_blob_storage");
   return NULL;
}

static inline bool
vkr_cs_handle_indirect_id(VkObjectType type)
{
   return parity_handle_indirect_id(type);
}

static inline vkr_object_id
vkr_cs_handle_load_id(const void **handle, VkObjectType type)
{
   return parity_handle_load_id(handle, type);
}

static inline void
vkr_cs_handle_store_id(void **handle, vkr_object_id id, VkObjectType type)
{
   parity_handle_store_id(handle, id, type);
}

#endif /* VKR_CS_H */

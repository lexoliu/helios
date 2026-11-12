/*
 * The command-stream primitives both generated Mesa headers expect, over a
 * caller-provided buffer. The generated code only encodes (driver headers)
 * and encodes replies (renderer headers); the buffer is zero-filled by the
 * caller and the padding after unaligned data is written as zero.
 */

#ifndef PARITY_CS_CORE_H
#define PARITY_CS_CORE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vulkan/vulkan.h>

struct parity_encoder {
   uint8_t *data;
   size_t capacity;
   size_t len;
   /* Set when the generated code wrote more than `capacity`; the bytes past
    * it are dropped and `len` still counts them, so the caller sees both
    * that the size was wrong and by how much. */
   bool overflow;
};

struct parity_decoder {
   const uint8_t *cur;
   const uint8_t *end;
   bool fatal;
};

static inline void
parity_encoder_write(struct parity_encoder *enc, size_t size, const void *val, size_t val_size)
{
   if (val_size > size) {
      fprintf(stderr, "parity: a %zu byte value written in %zu bytes\n", val_size, size);
      abort();
   }
   if (enc->len + size > enc->capacity) {
      enc->overflow = true;
      enc->len += size;
      return;
   }
   memcpy(enc->data + enc->len, val, val_size);
   memset(enc->data + enc->len + val_size, 0, size - val_size);
   enc->len += size;
}

static inline void
parity_decoder_read(struct parity_decoder *dec, size_t size, void *val, size_t val_size)
{
   if ((size_t)(dec->end - dec->cur) < size) {
      dec->fatal = true;
      memset(val, 0, val_size);
      return;
   }
   memcpy(val, dec->cur, val_size);
   dec->cur += size;
}

static inline void
parity_decoder_peek(const struct parity_decoder *dec, size_t size, void *val, size_t val_size)
{
   if ((size_t)(dec->end - dec->cur) < size) {
      memset(val, 0, val_size);
      return;
   }
   memcpy(val, dec->cur, val_size);
}

/* The handles that are pointers to an object whose first member is its id;
 * every other handle is its own id. */
static inline bool
parity_handle_indirect_id(VkObjectType type)
{
   switch (type) {
   case VK_OBJECT_TYPE_INSTANCE:
   case VK_OBJECT_TYPE_PHYSICAL_DEVICE:
   case VK_OBJECT_TYPE_DEVICE:
   case VK_OBJECT_TYPE_QUEUE:
   case VK_OBJECT_TYPE_COMMAND_BUFFER:
      return true;
   default:
      return false;
   }
}

static inline uint64_t
parity_handle_load_id(const void **handle, VkObjectType type)
{
   if (!*handle)
      return 0;
   if (parity_handle_indirect_id(type))
      return *(const uint64_t *)*handle;
   return (uint64_t)(uintptr_t)*handle;
}

static inline void
parity_handle_store_id(void **handle, uint64_t id, VkObjectType type)
{
   if (parity_handle_indirect_id(type))
      *(uint64_t *)*handle = id;
   else
      *handle = (void *)(uintptr_t)id;
}

/* The parts of the generated code a parity run never reaches (it decodes no
 * commands on the renderer side); reaching one is a bug of the harness. */
static inline void
parity_unreachable(const char *what)
{
   fprintf(stderr, "parity: %s is not part of what the parity run exercises\n", what);
   abort();
}

#endif /* PARITY_CS_CORE_H */

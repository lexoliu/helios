/* Entry points of Mesa's generated driver-side encoders, one set per command,
 * taking each argument as a pointer to its storage. */

#include "vn_protocol_driver.h"

uint32_t parity_api_version;
const uint8_t *parity_extension_enabled;
uint32_t parity_extension_count;

void
parity_set_protocol(uint32_t api_version, const uint8_t *enabled, uint32_t count)
{
   parity_api_version = api_version;
   parity_extension_enabled = enabled;
   parity_extension_count = count;
}
{% for command in commands %}
size_t
parity_sizeof_{{ command.name }}(void **a)
{
   return vn_sizeof_{{ command.name }}({% for arg in command.args %}{% if !loop.first %}, {% endif %}{{ arg }}{% endfor %});
}

size_t
parity_encode_{{ command.name }}(uint8_t *buffer, size_t capacity, uint32_t flags, void **a, int *overflow)
{
   struct vn_cs_encoder enc = { { buffer, capacity, 0, false } };
   vn_encode_{{ command.name }}(&enc, flags{% for arg in command.args %}, {{ arg }}{% endfor %});
   *overflow = enc.base.overflow;
   return enc.base.len;
}

size_t
parity_sizeof_reply_{{ command.name }}(void **a)
{
   return vn_sizeof_{{ command.name }}_reply({% for arg in command.args %}{% if !loop.first %}, {% endif %}{{ arg }}{% endfor %});
}
{% endfor %}

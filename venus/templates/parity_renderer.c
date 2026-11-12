/* Entry points of Mesa's generated renderer-side reply encoders, one per
 * command, taking each argument as a pointer to its storage and the return
 * value as up to eight bytes. */

#include "vn_protocol_renderer.h"
{% for command in commands %}
size_t
parity_encode_reply_{{ command.name }}(uint8_t *buffer, size_t capacity, uint64_t ret, void **a, int *overflow)
{
   struct vn_command_{{ command.name }} args;
   memset((void *)&args, 0, sizeof(args));
{% for field in command.fields %}   memcpy((void *)&args.{{ field }}, a[{{ loop.index0 }}], sizeof(args.{{ field }}));
{% endfor %}{% if command.has_return %}   memcpy((void *)&args.ret, &ret, sizeof(args.ret));
{% endif %}   struct vkr_cs_encoder enc = { { buffer, capacity, 0, false } };
   vn_encode_{{ command.name }}_reply((struct vn_cs_encoder *)&enc, &args);
   *overflow = enc.base.overflow;
   return enc.base.len;
}
{% endfor %}

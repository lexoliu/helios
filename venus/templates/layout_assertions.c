/* The size, alignment and member offsets the generator computes for a
 * pointer width of {{ pointer_width }} bytes, asserted by a C compiler for the target. */

#include <stddef.h>
#include <stdint.h>
#include <vulkan/vulkan.h>

_Static_assert(sizeof(void *) == {{ pointer_width }}, "the compiler targets a {{ pointer_width }}-byte pointer");
{% for aggregate in aggregates %}
_Static_assert(sizeof({{ aggregate.name }}) == {{ aggregate.size }}, "sizeof({{ aggregate.name }})");
_Static_assert(_Alignof({{ aggregate.name }}) == {{ aggregate.align }}, "alignof({{ aggregate.name }})");
{% for member in aggregate.members %}_Static_assert(offsetof({{ aggregate.name }}, {{ member.name }}) == {{ member.offset }}, "offsetof({{ aggregate.name }}, {{ member.name }})");
{% endfor %}{% endfor %}

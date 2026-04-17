MEMORY
{
  RAM : ORIGIN = 0x80200000, LENGTH = 1900M
}

REGION_ALIAS("REGION_TEXT", RAM);
REGION_ALIAS("REGION_RODATA", RAM);
REGION_ALIAS("REGION_DATA", RAM);
REGION_ALIAS("REGION_BSS", RAM);
REGION_ALIAS("REGION_HEAP", RAM);
REGION_ALIAS("REGION_STACK", RAM);

_stack_start = ORIGIN(RAM) + LENGTH(RAM);
_hart_stack_size = 1M;
_max_hart_id = 7;
_heap_size = 0;

SECTIONS
{
  .platform_meta : ALIGN(8)
  {
    __stack_bottom_value = .;
    QUAD(_stack_start - _hart_stack_size * (_max_hart_id + 1));
  } > REGION_RODATA
} INSERT AFTER .rodata;

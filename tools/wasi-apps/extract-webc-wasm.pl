#!/usr/bin/env perl
use strict;
use warnings;
use Fcntl qw(SEEK_SET);

my ($webc_path, $wasm_path) = @ARGV;
die "usage: extract-webc-wasm.pl <single-atom.webc> <out.wasm>\n"
    unless defined $webc_path && defined $wasm_path;

open my $in, '<:raw', $webc_path
    or die "failed to open $webc_path: $!\n";
local $/;
my $webc = <$in>;
close $in or die "failed to close $webc_path: $!\n";

die "$webc_path is not a WEBc v3 image\n"
    unless substr($webc, 0, 8) eq "\0webc003";

my $magic = index($webc, "\0asm");
die "$webc_path does not contain a wasm atom\n" if $magic < 8;

my $size = unpack('Q<', substr($webc, $magic - 8, 8));
die "$webc_path has an invalid wasm atom size $size\n"
    if $size < 8 || $magic + $size > length($webc);

my $wasm = substr($webc, $magic, $size);
die "$webc_path atom does not start with wasm magic\n"
    unless substr($wasm, 0, 4) eq "\0asm";

open my $out, '>:raw', $wasm_path
    or die "failed to open $wasm_path: $!\n";
print {$out} $wasm or die "failed to write $wasm_path: $!\n";
close $out or die "failed to close $wasm_path: $!\n";

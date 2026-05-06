use divan::counter::ItemsCount;
use divan::{AllocProfiler, Bencher, black_box};
use helios_hal::fs::FileRights;
use helios_hal::resource::KernelResource;
use helios_kernel::{DescriptorId, DescriptorTable};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

#[derive(Clone)]
struct BenchFile;

fn main() {
    divan::main();
}

#[divan::bench(args = [16usize, 64, 256])]
fn descriptor_insert_close_reuse(bencher: Bencher, count: usize) {
    descriptor_insert_close_reuse_with_table(bencher, count, DescriptorTable::new);
}

#[divan::bench(args = [16usize, 64, 256])]
fn descriptor_insert_close_reuse_preallocated(bencher: Bencher, count: usize) {
    descriptor_insert_close_reuse_with_table(bencher, count, || {
        DescriptorTable::with_capacity(count)
    });
}

fn descriptor_insert_close_reuse_with_table(
    bencher: Bencher,
    count: usize,
    new_table: impl Fn() -> DescriptorTable<BenchFile, FileRights>,
) {
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        let mut table = new_table();
        let mut descriptors = [DescriptorId::new(0); 256];
        for index in 0..count {
            descriptors[index] = table
                .insert(file(index as u64), false)
                .expect("descriptor insert should succeed");
        }
        for descriptor in descriptors.into_iter().take(count) {
            black_box(
                table
                    .close(descriptor)
                    .expect("descriptor close should succeed"),
            );
        }
    });
}

#[divan::bench]
fn descriptor_sparse_renumber_reuse(bencher: Bencher) {
    descriptor_sparse_renumber_reuse_with_table(bencher, DescriptorTable::new);
}

#[divan::bench]
fn descriptor_sparse_renumber_reuse_preallocated(bencher: Bencher) {
    descriptor_sparse_renumber_reuse_with_table(bencher, || DescriptorTable::with_capacity(256));
}

fn descriptor_sparse_renumber_reuse_with_table(
    bencher: Bencher,
    new_table: impl Fn() -> DescriptorTable<BenchFile, FileRights>,
) {
    bencher.bench_local(|| {
        let mut table = new_table();
        let descriptor = table
            .insert(file(0), false)
            .expect("descriptor insert should succeed");
        let target = DescriptorId::new(255);
        table
            .renumber(descriptor, target)
            .expect("sparse descriptor renumber should succeed");
        black_box(
            table
                .insert(file(1), false)
                .expect("descriptor insert should reuse sparse free slot"),
        );
    });
}

fn file(id: u64) -> KernelResource<BenchFile, FileRights> {
    black_box(id);
    let resource = KernelResource::new(BenchFile, FileRights::READ | FileRights::WRITE);
    black_box(resource)
}

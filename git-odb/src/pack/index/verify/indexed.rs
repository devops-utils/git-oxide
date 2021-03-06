use super::{Error, Mode, Outcome};
use crate::{
    pack,
    pack::index::access::PackOffset,
    pack::index::{self, verify::util},
};
use git_features::{
    parallel::{self, in_parallel_if},
    progress::{self, Progress},
};
use git_object::Kind;
use std::collections::BTreeMap;

impl index::File {
    pub(crate) fn inner_verify_with_indexed_lookup<P>(
        &self,
        thread_limit: Option<usize>,
        mode: Mode,
        mut root: progress::DoOrDiscard<P>,
        pack: &pack::data::File,
    ) -> Result<Outcome, Error>
    where
        P: Progress,
        <P as Progress>::SubProgress: Send,
    {
        let sorted_entries =
            util::index_entries_sorted_by_offset_ascending(self, root.add_child("collecting sorted index"));
        let tree = pack::graph::DeltaTree::from_sorted_offsets(
            sorted_entries.iter().map(|e| e.pack_offset),
            pack.path(),
            root.add_child("indexing"),
            |id| self.lookup_index(id).map(|idx| self.pack_offset_at_index(idx)),
        )?;
        let if_there_are_enough_objects = || self.num_objects > 10_000;

        let reduce_progress = std::sync::Mutex::new({
            let mut p = root.add_child("Checking");
            p.init(Some(self.num_objects()), Some("objects"));
            p
        });

        let state_per_thread = |index| {
            (
                Vec::<u8>::with_capacity(2048), // decode buffer
                Vec::<u8>::with_capacity(2048), // re-encode buffer
                Vec::<(pack::graph::Node, u32)>::new(),
                reduce_progress.lock().unwrap().add_child(format!("thread {}", index)), // per thread progress
            )
        };
        struct Chunks<I> {
            size: usize,
            iter: I,
        }
        impl<I> Iterator for Chunks<I>
        where
            I: Iterator<Item = pack::graph::Node>,
        {
            type Item = Vec<pack::graph::Node>;

            fn next(&mut self) -> Option<Self::Item> {
                let mut res = Vec::with_capacity(self.size);
                let mut items_left = self.size;
                while let Some(item) = self.iter.next() {
                    res.push(item);
                    items_left -= 1;
                    if items_left == 0 {
                        break;
                    }
                }
                if res.is_empty() {
                    None
                } else {
                    Some(res)
                }
            }
        }
        let (chunk_size, thread_limit, _) = parallel::optimize_chunk_size_and_thread_limit(1, None, thread_limit, None);
        in_parallel_if(
            if_there_are_enough_objects,
            Chunks {
                size: chunk_size,
                iter: tree.bases(),
            },
            thread_limit,
            state_per_thread,
            |input: Vec<pack::graph::Node>,
             (buf, encode_buf, nodes, progress)|
             -> Result<Vec<pack::data::decode::Outcome>, Error> {
                let mut stats = Vec::new();
                let mut header_buf = [0u8; 64];
                let mut children = Vec::new();
                progress.init(None, Some("entries"));

                struct CacheEntry {
                    kind: Kind,
                    data: Vec<u8>,
                    compressed_size: usize,
                }
                struct SharedCache<'a>(&'a mut BTreeMap<PackOffset, CacheEntry>);

                impl<'a> pack::cache::DecodeEntry for SharedCache<'a> {
                    fn put(&mut self, pack_offset: u64, data: &[u8], kind: Kind, compressed_size: usize) {
                        self.0.entry(pack_offset).or_insert_with(|| CacheEntry {
                            kind,
                            data: data.to_owned(),
                            compressed_size,
                        });
                    }

                    fn get(&mut self, offset: u64, out: &mut Vec<u8>) -> Option<(Kind, usize)> {
                        self.0.get_mut(&offset).map(
                            |CacheEntry {
                                 kind,
                                 data,
                                 compressed_size,
                             }| {
                                out.resize(data.len(), 0);
                                out.copy_from_slice(&data);
                                (*kind, *compressed_size)
                            },
                        )
                    }
                }

                for node in input {
                    nodes.clear();
                    nodes.push((node, 0));
                    let mut cache = BTreeMap::new();

                    while let Some((node, level)) = nodes.pop() {
                        let pack_offset = node.pack_offset;
                        let index_entry = sorted_entries
                            .binary_search_by(|e| e.pack_offset.cmp(&pack_offset))
                            .expect("tree created by our sorted entries");
                        let index_entry_of_node = &sorted_entries[index_entry];

                        tree.children(node, &mut children);

                        let shared_cache = &mut SharedCache(&mut cache);
                        let mut stat = self.process_entry(
                            mode,
                            pack,
                            shared_cache,
                            buf,
                            encode_buf,
                            progress,
                            &mut header_buf,
                            index_entry_of_node,
                        )?;
                        stat.num_deltas = level;
                        stats.push(stat);

                        progress.inc();
                        nodes.extend(children.iter().cloned().map(|cn| (cn, level + 1)));
                    }
                }

                Ok(stats)
            },
            index::verify::Reducer::from_progress(&reduce_progress, pack.data_len()),
        )
    }
}

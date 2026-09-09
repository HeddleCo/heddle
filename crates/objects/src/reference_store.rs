//! Canonical source-reference objects use the existing blob store.
use crate::{
    error::{HeddleError, Result},
    object::{
        Blob, ContentHash, ObjectSource, State, StateId, Tree,
        source_target_map::SourceTargetMapStore,
    },
    store::ObjectStore,
};
pub struct Source<'a, S>(pub &'a S);
impl<S: ObjectStore> ObjectSource for Source<'_, S> {
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        self.0.get_tree(hash)
    }
    fn get_state(&self, id: &StateId) -> Result<Option<State>> {
        self.0.get_state(id)
    }
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        self.0.get_blob(hash)
    }
    fn decoded_blob_len(&self, hash: &ContentHash) -> Result<Option<u64>> {
        self.0.blob_size(hash)
    }
}
pub struct MapStore<'a, S>(pub &'a S);
impl<S: ObjectStore> SourceTargetMapStore for MapStore<'_, S> {
    type Error = HeddleError;
    fn read(&mut self, hash: ContentHash, max: usize) -> Result<Option<Vec<u8>>> {
        if self.0.blob_size(&hash)?.is_some_and(|len| len > max as u64) {
            return Err(HeddleError::InvalidObject(
                "reference node read budget".into(),
            ));
        }
        Ok(self.0.get_blob(&hash)?.map(|blob| blob.into_content()))
    }
    fn write(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<()> {
        if ContentHash::compute_typed("blob", &bytes) != hash {
            return Err(HeddleError::InvalidObject(
                "reference node hash mismatch".into(),
            ));
        }
        self.0.put_blob(&Blob::new(bytes))?;
        Ok(())
    }
}

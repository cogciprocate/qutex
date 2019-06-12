use crate::*;
use futures::sync::oneshot::Canceled;
use futures03::compat::Future01CompatExt;

impl<T> Qutex<T> {
    pub async fn lock_async(self) -> Result<Guard<T>, Canceled> {
        self.lock().compat().await
    }
}

impl<T> QrwLock<T> {
    pub async fn write_async(self) -> Result<WriteGuard<T>, Canceled> {
        self.write().compat().await
    }

    pub async fn read_async(self) -> Result<ReadGuard<T>, Canceled> {
        self.read().compat().await
    }
}

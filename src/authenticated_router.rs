use iroh::{
	Endpoint, EndpointId,
	protocol::{AccessLimit, ProtocolHandler, Router, RouterBuilder},
};

/// Builder for creating an [`AuthenticatedRouter`] for accepting protocols with authentication enabled.
#[derive(Debug)]
pub(super) struct AuthenticatedRouterBuilder<T>
where
	T: Fn(EndpointId) -> bool + Send + Sync + 'static + Clone,
{
	inner: RouterBuilder,
	auther: T,
}

impl<T> AuthenticatedRouterBuilder<T>
where
	T: Fn(EndpointId) -> bool + Send + Sync + 'static + Clone,
{
	/// Creates a new authenticated router builder using given [`Endpoint`].
	pub(super) fn new(endpoint: Endpoint, auther: T) -> Self {
		Self {
			inner: RouterBuilder::new(endpoint),
			auther,
		}
	}

	/// Configures the router to accept the [`ProtocolHandler`] when receiving a connection
	/// with this `alpn`. The handler will reject connections for which the `auther` returns false.
	///
	/// `handler` can either be a type that implements [`ProtocolHandler`] or a
	/// [`Box<dyn DynProtocolHandler>`].
	///
	/// [`Box<dyn DynProtocolHandler>`]: DynProtocolHandler
	pub(super) fn accept(mut self, alpn: impl AsRef<[u8]>, handler: impl ProtocolHandler + Clone) -> Self {
		self.inner = self
			.inner
			.accept(alpn, AccessLimit::new(handler, self.auther.clone()));
		self
	}

	/// Spawns an accept loop and returns a handle to it encapsulated as the [`Router`].
	#[must_use = "Router aborts when dropped, use Router::shutdown to shut the router down cleanly"]
	pub(super) fn spawn(self) -> Router {
		self.inner.spawn()
	}
}

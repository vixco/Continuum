// Module shims for libraries without bundled types. Keeps the rest of
// the codebase strict — `force-graph` ships ESM types that model the
// factory as a class constructor, but the runtime is a callable instance.
// The MemoryGraph component uses its own structural type for the few
// methods it actually touches, so a minimal `any` here is safe and
// avoids a @types/* package we don't otherwise need.

declare module "force-graph" {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const ForceGraph: any;
  export default ForceGraph;
}

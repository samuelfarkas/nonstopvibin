// The hooks the suites use: Bun's node:test shim drops a hook's timeout option,
// so hooks that start or stop cores call these, which honour it. @types/bun is not a dependency.
declare module "bun:test" {
  export function beforeAll(fn: () => Promise<void>, timeout: number): void;
  export function afterAll(fn: () => Promise<void>, timeout: number): void;
}

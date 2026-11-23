// The clock control the recorder uses; @types/bun is not a dependency.
declare module "bun:test" {
  export function setSystemTime(now?: Date | number): void;
}

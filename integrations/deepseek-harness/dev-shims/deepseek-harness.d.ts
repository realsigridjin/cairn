declare const process: { readonly env: Record<string, string | undefined> }

declare module '@deepseek-ai/cordis' {
  export interface Context {
    readonly tools: { register(definition: unknown): void }
  }
}

declare module '@deepseek-ai/dsh-tools' {
  export interface ToolRunContext {
    readonly signal: AbortSignal
    readonly callId: string
  }
  export function defineTool<T>(definition: T): T
}

declare module '@deepseek-ai/schemastery' {
  export interface Schema<T> {
    required(): Schema<T>
    default(value: T): Schema<T>
    min(value: number): Schema<T>
    max(value: number): Schema<T>
    step(value: number): Schema<T>
  }
  export interface Factory {
    string(): Schema<string>
    number(): Schema<number>
    boolean(): Schema<boolean>
    array<T>(schema: Schema<T>): Schema<T[]>
    object<T>(shape: Record<string, Schema<unknown>>): Schema<T>
  }
  const z: Factory
  export type z<T> = Schema<T>
  export default z
}

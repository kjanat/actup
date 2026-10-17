import { initWasm } from "actup-wasm";
import { mock } from "bun:test";
import * as vscode from "./vscode.ts";

mock.module("vscode", () => vscode);

await initWasm();

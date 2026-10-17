const encoder = new TextEncoder();
const decoder = new TextDecoder();

export function byteToColumn(lineText: string, byteOffset: number): number {
	return decoder.decode(encoder.encode(lineText).subarray(0, byteOffset)).length;
}

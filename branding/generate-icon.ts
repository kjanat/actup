#!/usr/bin/env bun

/**
 * generate_icon - rasterize an SVG logo onto a colored canvas with optional rounded corners.
 *
 * Bun:
 *   bun branding/generate_icon.ts "#0d1117" branding/icon.svg branding/icon-128.png
 *   bun branding/generate_icon.ts -s 256 -p 15 "#0d1117" icon.svg icon-256.png
 *   bun branding/generate_icon.ts -r 20 none icon.svg icon-rounded.png
 */
/** biome-ignore-all assist/source/organizeImports: sorting with dprint */

import { arg, cli, CLIError, command, flag, generateInputSchema, generateSchema } from "@kjanat/dreamcli";
import { $ } from "bun";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

const maskStyles = ["polygon", "roundrect"] as const;
type MaskStyle = (typeof maskStyles)[number];

export const generateIcon = command("generate-icon")
	.arg("bg-color", arg.string().required().default("none").describe("Background color hex (e.g. \"#0d1117\") or \"none\" for transparent"))
	.arg("logo-path", arg.string().required().default(join(import.meta.dir, "icon.svg")).describe("Path to SVG logo"))
	.arg("output-path", arg.string().required().describe("Output PNG path"))
	.flag("size", flag.number().alias("s").default(128).describe("Canvas size in px"))
	.flag("padding", flag.number().alias("p").default(20).describe("Padding percentage"))
	.flag("radius", flag.number().alias("r").default(0).describe("Corner radius in px, 0 = square"))
	.flag(
		"mask",
		flag.enum([...maskStyles]).alias("m").default("polygon").describe(
			"Rounded-corner mask style: \"polygon\" (portable, IM 6+) or \"roundrect\" (cleaner, IM 7+)",
		),
	)
	.action(async ({ args, flags, out }) => {
		for (const [name, value] of [["size", flags.size], ["padding", flags.padding], ["radius", flags.radius]] as const) {
			if (!Number.isInteger(value) || value < 0) {
				throw new CLIError(`--${name} requires a non-negative integer (got ${value})`, { code: "INVALID_FLAG", exitCode: 1 });
			}
		}
		if (flags.size <= 0) {
			throw new CLIError(`--size must be greater than 0 (got ${flags.size})`, { code: "INVALID_FLAG", exitCode: 1 });
		}
		if (flags.padding >= 100) {
			throw new CLIError(`--padding must be less than 100 (got ${flags.padding})`, { code: "INVALID_FLAG", exitCode: 1 });
		}

		const { "bg-color": bgColor, "logo-path": logoPath, "output-path": outputPath } = args;
		const canvasSize = flags.size;
		const paddingPct = flags.padding;
		const cornerRadius = flags.radius;
		const maxLogoSize = Math.floor((canvasSize * (100 - paddingPct)) / 100);

		if (maxLogoSize <= 0) {
			throw new CLIError(`computed logo size must be greater than 0 (got ${maxLogoSize})`, { code: "INVALID_FLAG", exitCode: 1 });
		}

		const [magickBin, convertBin, rsvgBin] = await Promise.all([
			Bun.which("magick"),
			Bun.which("convert"),
			Bun.which("rsvg-convert"),
		]);

		if (!rsvgBin) {
			throw new CLIError("rsvg-convert not found", {
				code: "MISSING_DEP",
				exitCode: 1,
				suggest: "Install: brew install librsvg / apt install librsvg2-bin / scoop install librsvg",
			});
		}

		const imCmd = magickBin ? "magick" : convertBin ? "convert" : null;
		if (!imCmd) {
			throw new CLIError("ImageMagick not found (need `magick` or `convert`)", {
				code: "MISSING_DEP",
				exitCode: 1,
				suggest: "Install: brew install imagemagick / apt install imagemagick / scoop install imagemagick",
			});
		}

		if (!(await Bun.file(logoPath).exists())) {
			throw new CLIError(`file not found: ${logoPath}`, { code: "NOT_FOUND", exitCode: 1 });
		}
		await $`mkdir -p ${dirname(outputPath)}`.nothrow().quiet();

		const tmpDir = await mkdtemp(join(tmpdir(), "icon_logo_"));
		const tmpPng = join(tmpDir, "logo.png");

		try {
			out.log(`Generating ${outputPath} (${canvasSize}x${canvasSize}, padding ${paddingPct}%)...`);

			await $`rsvg-convert --width=${maxLogoSize} --height=${maxLogoSize} --keep-aspect-ratio ${logoPath} -o ${tmpPng}`;

			const composeArgs = buildComposeArgs({ canvasSize, bgColor, tmpPng, cornerRadius, maskStyle: flags.mask });
			await $`${imCmd} ${composeArgs} ${outputPath}`;

			out.log(`Done: ${outputPath}`);
		} finally {
			await rm(tmpDir, { recursive: true, force: true });
		}
	});

interface ComposeOpts {
	canvasSize: number;
	bgColor: string;
	tmpPng: string;
	cornerRadius: number;
	maskStyle: MaskStyle;
}

/**
 * Build ImageMagick args to composite the rasterized logo onto a colored
 * canvas, optionally masking with rounded corners.
 */
function buildComposeArgs(opts: ComposeOpts): string[] {
	const { canvasSize, bgColor, tmpPng, cornerRadius, maskStyle } = opts;
	const args: string[] = [
		"-size",
		`${canvasSize}x${canvasSize}`,
		`canvas:${bgColor}`,
		tmpPng,
		"-gravity",
		"center",
		"-composite",
	];

	if (cornerRadius > 0) {
		const maskArgs = maskStyle === "roundrect"
			? roundrectMaskArgs(canvasSize, cornerRadius)
			: polygonMaskArgs(cornerRadius);
		args.push(...maskArgs);
	}

	return args;
}

/**
 * Portable mask (IM 6+): extract the current image's alpha, stamp one rounded
 * corner with a polygon+circle, then mirror it via -flip/-flop multiplication
 * to cover all four corners before copying back as alpha.
 */
function polygonMaskArgs(radius: number): string[] {
	return [
		"(",
		"+clone",
		"-alpha",
		"extract",
		"-draw",
		`fill black polygon 0,0 0,${radius} ${radius},0 fill white circle ${radius},${radius} ${radius},0`,
		"(",
		"+clone",
		"-flip",
		")",
		"-compose",
		"Multiply",
		"-composite",
		"(",
		"+clone",
		"-flop",
		")",
		"-compose",
		"Multiply",
		"-composite",
		")",
		"-alpha",
		"off",
		"-compose",
		"CopyOpacity",
		"-composite",
	];
}

/**
 * Modern mask (IM 7+): build the mask in one draw call with the native
 * roundrectangle primitive, then copy it as alpha.
 */
function roundrectMaskArgs(canvasSize: number, radius: number): string[] {
	const last = canvasSize - 1;
	return [
		"(",
		"-size",
		`${canvasSize}x${canvasSize}`,
		"xc:black",
		"-fill",
		"white",
		"-draw",
		`roundrectangle 0,0 ${last},${last} ${radius},${radius}`,
		")",
		"-compose",
		"CopyOpacity",
		"-composite",
	];
}

export const emitSchema = command("emit-schema")
	.description("Write JSON schemas for this CLI next to generate_icon.ts and exit")
	.action(async ({ out }) => {
		const [full, input] = [generateSchema(app.schema), generateInputSchema(app.schema)];
		await Promise.all([
			Bun.write(`${import.meta.dir}/generate_icon.schema.json`, JSON.stringify(full)),
			Bun.write(`${import.meta.dir}/generate_icon.input.schema.json`, JSON.stringify(input)),
		]);
		out.log("Wrote generate_icon.schema.json and generate_icon.input.schema.json");
	});

export const app = cli("generate-icon")
	.packageJson({ inferName: true })
	.description("Rasterize an SVG logo onto a colored canvas (PNG output)")
	.default(generateIcon)
	.command(emitSchema);

if (import.meta.main) {
	app.run();
}

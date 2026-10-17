/**
 * @file GitHub Actions grammar for tree-sitter
 * @author Kaj Kowalski <info@kajkowalski.nl>
 * @license MIT
 * @see https://github.com/ikatyang/tree-sitter-yaml — original (Ika, MIT)
 */

/// <reference path="./types/dsl-augment.d.ts" />
/// <reference path="./node_modules/tree-sitter-cli/dsl.d.ts" />

/**
 * @typedef {{ name: string, shallow?: boolean }} AliasContent
 * @typedef {Record<string, AliasContent>} AliasMap
 * @typedef {Rule} RuleJson
 * @typedef {{ rules: Record<string, RuleJson>, [k: string]: unknown }} GrammarJson
 */

// /**
//  * @param {GrammarJson} grammarJson
//  * @return {Grammar}
//  */
const base = grammar({
	name: "github_actions",

	externals: $ => [
		$._eof,

		// s  = starts at column 0 in the current or following row
		// r  = starts with 0 or more s_white in the current row
		// br = starts with more than `current_indent` s_white in the following row
		// b  = starts with `current_indent` s_white in the following row
		// bl = starts with `parent_indent` or less s_white in the following row
		$._s_dir_yml_bgn,
		$._r_dir_yml_ver, // %YAML 1.2
		$._s_dir_tag_bgn,
		$._r_dir_tag_hdl,
		$._r_dir_tag_pfx, // %TAG !yaml! tag:yaml.org,2002:
		$._s_dir_rsv_bgn,
		$._r_dir_rsv_prm, // %FOO bar baz
		$._s_drs_end, // ---
		$._s_doc_end, // ...
		$._r_blk_seq_bgn,
		$._br_blk_seq_bgn,
		$._b_blk_seq_bgn, // -
		$._r_blk_key_bgn,
		$._br_blk_key_bgn,
		$._b_blk_key_bgn, // ?
		$._r_blk_val_bgn,
		$._br_blk_val_bgn,
		$._b_blk_val_bgn, // :
		$._r_blk_imp_bgn, // : (implicit)
		$._r_blk_lit_bgn,
		$._br_blk_lit_bgn, // |
		$._r_blk_fld_bgn,
		$._br_blk_fld_bgn, // >
		$._br_blk_str_ctn, // block scalar content
		$._r_flw_seq_bgn,
		$._br_flw_seq_bgn,
		$._b_flw_seq_bgn, // [
		$._r_flw_seq_end,
		$._br_flw_seq_end,
		$._b_flw_seq_end, // ]
		$._r_flw_map_bgn,
		$._br_flw_map_bgn,
		$._b_flw_map_bgn, // {
		$._r_flw_map_end,
		$._br_flw_map_end,
		$._b_flw_map_end, // }
		$._r_flw_sep_bgn,
		$._br_flw_sep_bgn, // ,
		$._r_flw_key_bgn,
		$._br_flw_key_bgn, // ?
		$._r_flw_jsv_bgn,
		$._br_flw_jsv_bgn, // : (json key)
		$._r_flw_njv_bgn,
		$._br_flw_njv_bgn, // : (non-json key)
		$._r_dqt_str_bgn,
		$._br_dqt_str_bgn,
		$._b_dqt_str_bgn, // " (start)
		$._r_dqt_str_ctn,
		$._br_dqt_str_ctn, // double quote scalar content
		$._r_dqt_esc_nwl,
		$._br_dqt_esc_nwl, // escape newline
		$._r_dqt_esc_seq,
		$._br_dqt_esc_seq, // escape sequence
		$._r_dqt_str_end,
		$._br_dqt_str_end, // " (end)
		$._r_sqt_str_bgn,
		$._br_sqt_str_bgn,
		$._b_sqt_str_bgn, // ' (start)
		$._r_sqt_str_ctn,
		$._br_sqt_str_ctn, // single quote scalar content
		$._r_sqt_esc_sqt,
		$._br_sqt_esc_sqt, // ''
		$._r_sqt_str_end,
		$._br_sqt_str_end, // ' (end)

		// plain scalar (singleline in block/flow)
		$._r_sgl_pln_nul_blk,
		$._br_sgl_pln_nul_blk,
		$._b_sgl_pln_nul_blk,
		$._r_sgl_pln_nul_flw,
		$._br_sgl_pln_nul_flw,
		$._r_sgl_pln_bol_blk,
		$._br_sgl_pln_bol_blk,
		$._b_sgl_pln_bol_blk,
		$._r_sgl_pln_bol_flw,
		$._br_sgl_pln_bol_flw,
		$._r_sgl_pln_int_blk,
		$._br_sgl_pln_int_blk,
		$._b_sgl_pln_int_blk,
		$._r_sgl_pln_int_flw,
		$._br_sgl_pln_int_flw,
		$._r_sgl_pln_flt_blk,
		$._br_sgl_pln_flt_blk,
		$._b_sgl_pln_flt_blk,
		$._r_sgl_pln_flt_flw,
		$._br_sgl_pln_flt_flw,
		$._r_sgl_pln_str_blk,
		$._br_sgl_pln_str_blk,
		$._b_sgl_pln_str_blk,
		$._r_sgl_pln_str_flw,
		$._br_sgl_pln_str_flw,

		// plain scalar (multiline in block/flow)
		$._r_mtl_pln_str_blk,
		$._br_mtl_pln_str_blk,
		$._r_mtl_pln_str_flw,
		$._br_mtl_pln_str_flw,

		$._r_tag,
		$._br_tag,
		$._b_tag, // !tag
		$._r_acr_bgn,
		$._br_acr_bgn,
		$._b_acr_bgn,
		$._r_acr_ctn, // &id
		$._r_als_bgn,
		$._br_als_bgn,
		$._b_als_bgn,
		$._r_als_ctn, // *id

		$._bl,
		$.comment,

		$._err_rec,

		$._expr_bgn, // `${{` (GitHub Actions expression open) — emitted by scanner before scalar content
		$._r_if_expr_bgn,
	],

	extras: $ => [$.comment],

	conflicts: $ => [
		[$._r_prp, $._r_sgl_prp],
		[$._br_prp, $._br_sgl_prp],
		[$._flw_seq_tal, $._sgl_flw_seq_tal],
		[$._flw_map_tal, $._sgl_flw_map_tal],
		[$._flw_ann_par_tal, $._sgl_flw_ann_par_tal],
		[$._r_flw_seq_itm, $._r_sgl_flw_col_itm],
		[$._r_flw_map_itm, $._r_sgl_flw_col_itm],
		[$._r_flw_njl_ann_par, $._r_sgl_flw_njl_ann_par],
		[$._r_flw_exp_par, $._r_sgl_flw_exp_par],
		[$._r_dqt_str, $._r_sgl_dqt_str],
		[$._r_sqt_str, $._r_sgl_sqt_str],
		[$._r_pln_flw_val, $._r_sgl_pln_flw_val],

		/**
		 * (_r_prp  _r_acr  •  _br_tag)
		 *
		 *    &str
		 *    !!str a
		 *
		 * (_r_prp  _r_acr)  •  _br_tag
		 *
		 *    &map
		 *    !!str a: 1
		 */
		[$._r_prp],
		[$._br_prp],

		// Allow GLR disambiguation when a `${{` (delimited_expression) appears inside multiline
		// quoted scalars: after a content chunk, continuing the same chunk-sequence vs. starting
		// a new one for the expression are both valid until one branch fails.
		[$._br_mtl_dqt_ctn],
		[$._br_mtl_sqt_ctn],

		// Single-line vs multi-line scalar disambiguation when an expression is the first content.
		[$._r_dqt_str, $._r_sgl_dqt_str, $._br_mtl_dqt_ctn],
		[$._r_sqt_str, $._r_sgl_sqt_str, $._br_mtl_sqt_ctn],

		// Trailing _expr_ws after the last argument vs. continuation of arguments list.
		[$.arguments],
		[$._format_arguments],

		// Plain-scalar with embedded `delimited_expression` introduces ambiguity at
		// every block-context entry point and between sgl/mtl variants. GLR resolves
		// at runtime once `}}` (or content) commits one branch.
		[$._r_blk_key_itm],
		[$._br_blk_key_itm],
		[$._b_blk_key_itm],
		[$._r_blk_val_itm],
		[$._br_blk_val_itm],
		[$._b_blk_val_itm],
		[$._r_blk_seq_itm],
		[$._br_blk_seq_itm],
		[$._b_blk_seq_itm],
		[$._blk_imp_itm_tal],
		[$._r_pln_blk_val, $._br_pln_blk_val, $._r_sgl_pln_blk, $._br_sgl_pln_blk],
		[$._r_pln_flw_val, $._br_pln_flw_val, $._r_sgl_pln_flw, $._br_sgl_pln_flw],
		[$._r_sgl_pln_blk, $._br_sgl_pln_blk, $._r_mtl_pln_blk, $._br_mtl_pln_blk],
		[$._r_sgl_pln_flw, $._br_sgl_pln_flw, $._r_mtl_pln_flw, $._br_mtl_pln_flw],
		[$._r_prp_val, $._r_blk_map_br_val, $._r_pln_blk_val],
		[$._br_prp_val, $._br_blk_map_val, $._br_pln_blk_val],
		[$._r_sgl_pln_blk, $._br_sgl_pln_blk],
		[$._r_sgl_pln_flw, $._br_sgl_pln_flw],
		[$._r_sgl_pln_blk, $._r_mtl_pln_blk],
		[$._br_sgl_pln_blk, $._br_mtl_pln_blk],
		[$._r_sgl_pln_flw, $._r_mtl_pln_flw],
		[$._br_sgl_pln_flw, $._br_mtl_pln_flw],
	],

	inline: $ => [
		$._r_pln_blk,
		$._br_pln_blk,
		$._r_pln_flw,
		$._br_pln_flw,
		$._r_blk_seq_val,
		$._r_blk_map_val,
		$._r_flw_val_blk,
		$._br_flw_val_blk,
		$._r_sgl_flw_val_blk,
		$._br_sgl_flw_val_blk,
		$._b_sgl_flw_val_blk,
		$._r_flw_val_flw,
		$._br_flw_val_flw,
		$._r_sgl_flw_val_flw,
		$._r_flw_jsl_val,
		$._br_flw_jsl_val,
		$._r_sgl_flw_jsl_val,
		$._br_sgl_flw_jsl_val,
		$._b_sgl_flw_jsl_val,
		$._r_flw_njl_val_blk,
		$._br_flw_njl_val_blk,
		$._r_sgl_flw_njl_val_blk,
		$._br_sgl_flw_njl_val_blk,
		$._b_sgl_flw_njl_val_blk,
		$._r_flw_njl_val_flw,
		$._br_flw_njl_val_flw,
		$._r_sgl_flw_njl_val_flw,
	],

	rules: {
		stream: $ =>
			seq(
				optional(choice(
					seq(
						choice($._bgn_imp_doc, $._drs_doc, $._exp_doc),
						optional(choice($._doc_w_bgn_w_end_seq, $._doc_w_bgn_wo_end_seq)),
					),
					seq(
						choice($._bgn_imp_doc_end, $._drs_doc_end, $._exp_doc_end, $._doc_end),
						optional(choice($._doc_w_bgn_w_end_seq, $._doc_w_bgn_wo_end_seq, $._doc_wo_bgn_w_end_seq, $._doc_wo_bgn_wo_end_seq)),
					),
				)),
				$._eof,
			),

		_doc_w_bgn_w_end_seq: $ =>
			seq(
				$._doc_w_bgn_w_end,
				optional(choice($._doc_w_bgn_w_end_seq, $._doc_w_bgn_wo_end_seq, $._doc_wo_bgn_w_end_seq, $._doc_wo_bgn_wo_end_seq)),
			),
		_doc_w_bgn_wo_end_seq: $ => seq($._doc_w_bgn_wo_end, optional(choice($._doc_w_bgn_w_end_seq, $._doc_w_bgn_wo_end_seq))),
		_doc_wo_bgn_w_end_seq: $ =>
			seq(
				$._doc_wo_bgn_w_end,
				optional(choice($._doc_w_bgn_w_end_seq, $._doc_w_bgn_wo_end_seq, $._doc_wo_bgn_w_end_seq, $._doc_wo_bgn_wo_end_seq)),
			),
		_doc_wo_bgn_wo_end_seq: $ => seq($._doc_wo_bgn_wo_end, optional(choice($._doc_w_bgn_w_end_seq, $._doc_w_bgn_wo_end_seq))),

		_doc_w_bgn_w_end: $ => choice($._exp_doc_end, $._doc_end),
		_doc_w_bgn_wo_end: $ => $._exp_doc,
		_doc_wo_bgn_w_end: $ => choice($._drs_doc_end, $._imp_doc_end),
		_doc_wo_bgn_wo_end: $ => choice($._drs_doc, $._imp_doc),

		// document

		_bgn_imp_doc: $ => choice($._exp_doc_tal, $._r_blk_seq_r_val, $._r_blk_map_r_val),
		_drs_doc: $ => seq(repeat1($._s_dir), $._exp_doc),
		_exp_doc: $ => seq($._s_drs_end, optional($._exp_doc_tal)),
		_imp_doc: $ => choice($._br_blk_seq_val, $._br_blk_map_val, $._br_blk_str_val, $._br_flw_val_blk),

		_drs_doc_end: $ => prec(1, seq($._drs_doc, $._s_doc_end)),
		_exp_doc_end: $ => prec(1, seq($._exp_doc, $._s_doc_end)),
		_imp_doc_end: $ => prec(1, seq($._imp_doc, $._s_doc_end)),
		_bgn_imp_doc_end: $ => prec(1, seq($._bgn_imp_doc, $._s_doc_end)),
		_doc_end: $ => $._s_doc_end,

		_exp_doc_tal: $ =>
			choice(
				$._r_blk_seq_br_val,
				$._br_blk_seq_val,
				$._r_blk_map_br_val,
				$._br_blk_map_val,
				$._r_blk_str_val,
				$._br_blk_str_val,
				$._r_flw_val_blk,
				$._br_flw_val_blk,
			),

		// directive

		_s_dir: $ => choice($._s_dir_yml, $._s_dir_tag, $._s_dir_rsv),

		_s_dir_yml: $ => seq($._s_dir_yml_bgn, $._r_dir_yml_ver),
		_s_dir_tag: $ => seq($._s_dir_tag_bgn, $._r_dir_tag_hdl, $._r_dir_tag_pfx),
		_s_dir_rsv: $ => seq($._s_dir_rsv_bgn, repeat($._r_dir_rsv_prm)),

		// property

		_r_prp_val: $ => $._r_prp,
		_br_prp_val: $ => $._br_prp,

		_r_sgl_prp_val: $ => $._r_sgl_prp,
		_br_sgl_prp_val: $ => $._br_sgl_prp,
		_b_sgl_prp_val: $ => $._b_sgl_prp,

		_r_prp: $ => choice(seq($._r_acr, optional(choice($._r_tag, $._br_tag))), seq($._r_tag, optional(choice($._r_acr, $._br_acr)))),
		_br_prp: $ => choice(seq($._br_acr, optional(choice($._r_tag, $._br_tag))), seq($._br_tag, optional(choice($._r_acr, $._br_acr)))),

		_r_sgl_prp: $ => choice(seq($._r_acr, optional($._r_tag)), seq($._r_tag, optional($._r_acr))),
		_br_sgl_prp: $ => choice(seq($._br_acr, optional($._r_tag)), seq($._br_tag, optional($._r_acr))),
		_b_sgl_prp: $ => choice(seq($._b_acr, optional($._r_tag)), seq($._b_tag, optional($._r_acr))),

		// block sequence

		_r_blk_seq_val: $ => choice($._r_blk_seq_r_val, $._r_blk_seq_br_val),
		_r_blk_seq_r_val: $ => $._r_blk_seq,
		_r_blk_seq_br_val: $ => seq($._r_prp, $._br_blk_seq),
		_br_blk_seq_val: $ => choice($._br_blk_seq, seq($._br_prp, $._br_blk_seq)),

		_r_blk_seq_spc_val: $ => seq($._r_prp, $._b_blk_seq_spc),
		_br_blk_seq_spc_val: $ => seq($._br_prp, $._b_blk_seq_spc),
		_b_blk_seq_spc_val: $ => $._b_blk_seq_spc,

		_r_blk_seq: $ => seq($._r_blk_seq_itm, repeat($._b_blk_seq_itm), $._bl),
		_br_blk_seq: $ => seq($._br_blk_seq_itm, repeat($._b_blk_seq_itm), $._bl),

		_b_blk_seq_spc: $ => seq(repeat1($._b_blk_seq_itm), $._bl),

		_r_blk_seq_itm: $ => seq($._r_blk_seq_bgn, optional($._blk_seq_itm_tal)),
		_br_blk_seq_itm: $ => seq($._br_blk_seq_bgn, optional($._blk_seq_itm_tal)),
		_b_blk_seq_itm: $ => seq($._b_blk_seq_bgn, optional($._blk_seq_itm_tal)),

		_blk_seq_itm_tal: $ =>
			choice(
				$._r_blk_seq_val,
				$._br_blk_seq_val,
				$._r_blk_map_val,
				$._br_blk_map_val,
				$._r_blk_str_val,
				$._br_blk_str_val,
				$._r_flw_val_blk,
				$._br_flw_val_blk,
			),

		// block mapping

		_r_blk_map_val: $ => choice($._r_blk_map_r_val, $._r_blk_map_br_val),
		_r_blk_map_r_val: $ => $._r_blk_map,
		_r_blk_map_br_val: $ => seq($._r_prp, $._br_blk_map),
		_br_blk_map_val: $ => choice($._br_blk_map, seq($._br_prp, $._br_blk_map)),

		_r_blk_map: $ => seq($._r_blk_map_itm, repeat($._b_blk_map_itm), $._bl),
		_br_blk_map: $ => seq($._br_blk_map_itm, repeat($._b_blk_map_itm), $._bl),

		_r_blk_map_itm: $ => choice($._r_blk_exp_itm, $._r_blk_imp_itm),
		_br_blk_map_itm: $ => choice($._br_blk_exp_itm, $._br_blk_imp_itm),
		_b_blk_map_itm: $ => choice($._b_blk_exp_itm, $._b_blk_imp_itm),

		_r_blk_exp_itm: $ => prec.right(choice(seq($._r_blk_key_itm, optional($._b_blk_val_itm)), $._r_blk_val_itm)),
		_br_blk_exp_itm: $ => prec.right(choice(seq($._br_blk_key_itm, optional($._b_blk_val_itm)), $._br_blk_val_itm)),
		_b_blk_exp_itm: $ => prec.right(choice(seq($._b_blk_key_itm, optional($._b_blk_val_itm)), $._b_blk_val_itm)),

		_r_blk_key_itm: $ => seq($._r_blk_key_bgn, optional(field("key", $._blk_exp_itm_tal))),
		_br_blk_key_itm: $ => seq($._br_blk_key_bgn, optional(field("key", $._blk_exp_itm_tal))),
		_b_blk_key_itm: $ => seq($._b_blk_key_bgn, optional(field("key", $._blk_exp_itm_tal))),

		_r_blk_val_itm: $ => seq($._r_blk_val_bgn, optional(field("value", $._blk_exp_itm_tal))),
		_br_blk_val_itm: $ => seq($._br_blk_val_bgn, optional(field("value", $._blk_exp_itm_tal))),
		_b_blk_val_itm: $ => seq($._b_blk_val_bgn, optional(field("value", $._blk_exp_itm_tal))),

		_r_blk_imp_itm: $ => seq(field("key", $._r_sgl_flw_val_blk), $._blk_imp_itm_tal),
		_br_blk_imp_itm: $ => seq(field("key", $._br_sgl_flw_val_blk), $._blk_imp_itm_tal),
		_b_blk_imp_itm: $ => seq(field("key", $._b_sgl_flw_val_blk), $._blk_imp_itm_tal),

		_blk_exp_itm_tal: $ => choice($._blk_seq_itm_tal, $._r_blk_seq_spc_val, $._br_blk_seq_spc_val, $._b_blk_seq_spc_val),
		_blk_imp_itm_tal: $ =>
			seq(
				$._r_blk_imp_bgn,
				optional(
					field(
						"value",
						choice(
							$._r_blk_seq_br_val,
							$._br_blk_seq_val,
							$._r_blk_seq_spc_val,
							$._br_blk_seq_spc_val,
							$._b_blk_seq_spc_val,
							$._r_blk_map_br_val,
							$._br_blk_map_val,
							$._r_blk_str_val,
							$._br_blk_str_val,
							$._r_flw_val_blk,
							$._br_flw_val_blk,
							$._r_if_expr_val,
						),
					),
				),
			),

		_r_if_expr_val: $ => seq($._r_if_expr_bgn, $.expression),

		// block scalar

		_r_blk_str_val: $ => choice($._r_blk_str, seq($._r_prp, choice($._r_blk_str, $._br_blk_str))),
		_br_blk_str_val: $ => choice($._br_blk_str, seq($._br_prp, choice($._r_blk_str, $._br_blk_str))),

		_r_blk_str: $ => seq(choice($._r_blk_lit_bgn, $._r_blk_fld_bgn), repeat(choice($._br_blk_str_ctn, $.delimited_expression)), $._bl),
		_br_blk_str: $ => seq(choice($._br_blk_lit_bgn, $._br_blk_fld_bgn), repeat(choice($._br_blk_str_ctn, $.delimited_expression)), $._bl),

		// flow value in block

		_r_flw_val_blk: $ => choice($._r_flw_jsl_val, $._r_flw_njl_val_blk),
		_br_flw_val_blk: $ => choice($._br_flw_jsl_val, $._br_flw_njl_val_blk),

		_r_sgl_flw_val_blk: $ => choice($._r_sgl_flw_jsl_val, $._r_sgl_flw_njl_val_blk),
		_br_sgl_flw_val_blk: $ => choice($._br_sgl_flw_jsl_val, $._br_sgl_flw_njl_val_blk),
		_b_sgl_flw_val_blk: $ => choice($._b_sgl_flw_jsl_val, $._b_sgl_flw_njl_val_blk),

		// flow value in flow

		_r_flw_val_flw: $ => choice($._r_flw_jsl_val, $._r_flw_njl_val_flw),
		_br_flw_val_flw: $ => choice($._br_flw_jsl_val, $._br_flw_njl_val_flw),

		_r_sgl_flw_val_flw: $ => choice($._r_sgl_flw_jsl_val, $._r_sgl_flw_njl_val_flw),

		// json-like flow value

		_r_flw_jsl_val: $ => choice($._r_flw_seq_val, $._r_flw_map_val, $._r_dqt_str_val, $._r_sqt_str_val),
		_br_flw_jsl_val: $ => choice($._br_flw_seq_val, $._br_flw_map_val, $._br_dqt_str_val, $._br_sqt_str_val),

		_r_sgl_flw_jsl_val: $ => choice($._r_sgl_flw_seq_val, $._r_sgl_flw_map_val, $._r_sgl_dqt_str_val, $._r_sgl_sqt_str_val),
		_br_sgl_flw_jsl_val: $ => choice($._br_sgl_flw_seq_val, $._br_sgl_flw_map_val, $._br_sgl_dqt_str_val, $._br_sgl_sqt_str_val),
		_b_sgl_flw_jsl_val: $ => choice($._b_sgl_flw_seq_val, $._b_sgl_flw_map_val, $._b_sgl_dqt_str_val, $._b_sgl_sqt_str_val),

		// non-json-like flow value in block

		_r_flw_njl_val_blk: $ => choice($._r_als_val, $._r_prp_val, $._r_pln_blk_val),
		_br_flw_njl_val_blk: $ => choice($._br_als_val, $._br_prp_val, $._br_pln_blk_val),

		_r_sgl_flw_njl_val_blk: $ => choice($._r_als_val, $._r_sgl_prp_val, $._r_sgl_pln_blk_val),
		_br_sgl_flw_njl_val_blk: $ => choice($._br_als_val, $._br_sgl_prp_val, $._br_sgl_pln_blk_val),
		_b_sgl_flw_njl_val_blk: $ => choice($._b_als_val, $._b_sgl_prp_val, $._b_sgl_pln_blk_val),

		// non-json-like flow value in flow

		_r_flw_njl_val_flw: $ => choice($._r_als_val, $._r_prp_val, $._r_pln_flw_val),
		_br_flw_njl_val_flw: $ => choice($._br_als_val, $._br_prp_val, $._br_pln_flw_val),

		_r_sgl_flw_njl_val_flw: $ => choice($._r_als_val, $._r_sgl_prp_val, $._r_sgl_pln_flw_val),

		// flow sequence

		_r_flw_seq_val: $ => choice($._r_flw_seq, seq($._r_prp, choice($._r_flw_seq, $._br_flw_seq))),
		_br_flw_seq_val: $ => choice($._br_flw_seq, seq($._br_prp, choice($._r_flw_seq, $._br_flw_seq))),

		_r_sgl_flw_seq_val: $ => choice($._r_sgl_flw_seq, seq($._r_sgl_prp, $._r_sgl_flw_seq)),
		_br_sgl_flw_seq_val: $ => choice($._br_sgl_flw_seq, seq($._br_sgl_prp, $._r_sgl_flw_seq)),
		_b_sgl_flw_seq_val: $ => choice($._b_sgl_flw_seq, seq($._b_sgl_prp, $._r_sgl_flw_seq)),

		_r_flw_seq: $ => seq($._r_flw_seq_bgn, $._flw_seq_tal),
		_br_flw_seq: $ => seq($._br_flw_seq_bgn, $._flw_seq_tal),

		_r_sgl_flw_seq: $ => seq($._r_flw_seq_bgn, $._sgl_flw_seq_tal),
		_br_sgl_flw_seq: $ => seq($._br_flw_seq_bgn, $._sgl_flw_seq_tal),
		_b_sgl_flw_seq: $ => seq($._b_flw_seq_bgn, $._sgl_flw_seq_tal),

		_flw_seq_tal: $ =>
			seq(optional(choice($._r_flw_seq_dat, $._br_flw_seq_dat)), choice($._r_flw_seq_end, $._br_flw_seq_end, $._b_flw_seq_end)),
		_sgl_flw_seq_tal: $ => seq(optional($._r_sgl_flw_col_dat), $._r_flw_seq_end),

		// flow mapping

		_r_flw_map_val: $ => choice($._r_flw_map, seq($._r_prp, choice($._r_flw_map, $._br_flw_map))),
		_br_flw_map_val: $ => choice($._br_flw_map, seq($._br_prp, choice($._r_flw_map, $._br_flw_map))),

		_r_sgl_flw_map_val: $ => choice($._r_sgl_flw_map, seq($._r_sgl_prp, $._r_sgl_flw_map)),
		_br_sgl_flw_map_val: $ => choice($._br_sgl_flw_map, seq($._br_sgl_prp, $._r_sgl_flw_map)),
		_b_sgl_flw_map_val: $ => choice($._b_sgl_flw_map, seq($._b_sgl_prp, $._r_sgl_flw_map)),

		_r_flw_map: $ => seq($._r_flw_map_bgn, $._flw_map_tal),
		_br_flw_map: $ => seq($._br_flw_map_bgn, $._flw_map_tal),

		_r_sgl_flw_map: $ => seq($._r_flw_map_bgn, $._sgl_flw_map_tal),
		_br_sgl_flw_map: $ => seq($._br_flw_map_bgn, $._sgl_flw_map_tal),
		_b_sgl_flw_map: $ => seq($._b_flw_map_bgn, $._sgl_flw_map_tal),

		_flw_map_tal: $ =>
			seq(optional(choice($._r_flw_map_dat, $._br_flw_map_dat)), choice($._r_flw_map_end, $._br_flw_map_end, $._b_flw_map_end)),
		_sgl_flw_map_tal: $ => seq(optional($._r_sgl_flw_col_dat), $._r_flw_map_end),

		// flow collection data

		_r_flw_seq_dat: $ => seq($._r_flw_seq_itm, repeat($._flw_seq_dat_rpt), optional(choice($._r_flw_sep_bgn, $._br_flw_sep_bgn))),
		_br_flw_seq_dat: $ => seq($._br_flw_seq_itm, repeat($._flw_seq_dat_rpt), optional(choice($._r_flw_sep_bgn, $._br_flw_sep_bgn))),

		_r_flw_map_dat: $ => seq($._r_flw_map_itm, repeat($._flw_map_dat_rpt), optional(choice($._r_flw_sep_bgn, $._br_flw_sep_bgn))),
		_br_flw_map_dat: $ => seq($._br_flw_map_itm, repeat($._flw_map_dat_rpt), optional(choice($._r_flw_sep_bgn, $._br_flw_sep_bgn))),

		_r_sgl_flw_col_dat: $ => seq($._r_sgl_flw_col_itm, repeat($._sgl_flw_col_dat_rpt), optional($._r_flw_sep_bgn)),

		_flw_seq_dat_rpt: $ => seq(choice($._r_flw_sep_bgn, $._br_flw_sep_bgn), choice($._r_flw_seq_itm, $._br_flw_seq_itm)),
		_flw_map_dat_rpt: $ => seq(choice($._r_flw_sep_bgn, $._br_flw_sep_bgn), choice($._r_flw_map_itm, $._br_flw_map_itm)),

		_sgl_flw_col_dat_rpt: $ => seq($._r_flw_sep_bgn, $._r_sgl_flw_col_itm),

		// flow collection item

		_r_flw_seq_itm: $ => choice($._r_flw_val_flw, $._r_flw_exp_par, $._r_flw_imp_r_par, $._r_flw_njl_ann_par),
		_br_flw_seq_itm: $ => choice($._br_flw_val_flw, $._br_flw_exp_par, $._br_flw_imp_r_par, $._br_flw_njl_ann_par),

		_r_flw_map_itm: $ => choice($._r_flw_val_flw, $._r_flw_exp_par, $._r_flw_imp_r_par, $._r_flw_imp_br_par, $._r_flw_njl_ann_par),
		_br_flw_map_itm: $ => choice($._br_flw_val_flw, $._br_flw_exp_par, $._br_flw_imp_r_par, $._br_flw_imp_br_par, $._br_flw_njl_ann_par),

		_r_sgl_flw_col_itm: $ => choice($._r_sgl_flw_val_flw, $._r_sgl_flw_exp_par, $._r_sgl_flw_imp_par, $._r_sgl_flw_njl_ann_par),

		// explicit flow pair

		_r_flw_exp_par: $ =>
			seq($._r_flw_key_bgn, optional(choice($._r_flw_imp_r_par, $._r_flw_imp_br_par, $._br_flw_imp_r_par, $._br_flw_imp_br_par))),
		_br_flw_exp_par: $ =>
			seq($._br_flw_key_bgn, optional(choice($._r_flw_imp_r_par, $._r_flw_imp_br_par, $._br_flw_imp_r_par, $._br_flw_imp_br_par))),

		_r_sgl_flw_exp_par: $ => seq($._r_flw_key_bgn, optional($._r_sgl_flw_imp_par)),

		// implicit flow pair

		_r_flw_imp_r_par: $ =>
			choice(seq(field("key", $._r_flw_jsl_val), $._r_flw_jsl_ann_par), seq(field("key", $._r_flw_njl_val_flw), $._r_flw_njl_ann_par)),
		_r_flw_imp_br_par: $ =>
			choice(seq(field("key", $._r_flw_jsl_val), $._br_flw_jsl_ann_par), seq(field("key", $._r_flw_njl_val_flw), $._br_flw_njl_ann_par)),
		_br_flw_imp_r_par: $ =>
			choice(seq(field("key", $._br_flw_jsl_val), $._r_flw_jsl_ann_par), seq(field("key", $._br_flw_njl_val_flw), $._r_flw_njl_ann_par)),
		_br_flw_imp_br_par: $ =>
			choice(seq(field("key", $._br_flw_jsl_val), $._br_flw_jsl_ann_par), seq(field("key", $._br_flw_njl_val_flw), $._br_flw_njl_ann_par)),

		_r_sgl_flw_imp_par: $ =>
			choice(
				seq(field("key", $._r_sgl_flw_jsl_val), $._r_sgl_flw_jsl_ann_par),
				seq(field("key", $._r_sgl_flw_njl_val_flw), $._r_sgl_flw_njl_ann_par),
			),

		// anonymous flow pair

		_r_flw_jsl_ann_par: $ => seq($._r_flw_jsv_bgn, optional(field("value", $._flw_ann_par_tal))),
		_br_flw_jsl_ann_par: $ => seq($._br_flw_jsv_bgn, optional(field("value", $._flw_ann_par_tal))),

		_r_sgl_flw_jsl_ann_par: $ => seq($._r_flw_jsv_bgn, optional(field("value", $._sgl_flw_ann_par_tal))),

		_r_flw_njl_ann_par: $ => seq($._r_flw_njv_bgn, optional(field("value", $._flw_ann_par_tal))),
		_br_flw_njl_ann_par: $ => seq($._br_flw_njv_bgn, optional(field("value", $._flw_ann_par_tal))),

		_r_sgl_flw_njl_ann_par: $ => seq($._r_flw_njv_bgn, optional(field("value", $._sgl_flw_ann_par_tal))),

		_flw_ann_par_tal: $ => choice($._r_flw_val_flw, $._br_flw_val_flw),
		_sgl_flw_ann_par_tal: $ => $._r_sgl_flw_val_flw,

		// double quote scalar

		_r_dqt_str_val: $ => choice($._r_dqt_str, seq($._r_prp, choice($._r_dqt_str, $._br_dqt_str))),
		_br_dqt_str_val: $ => choice($._br_dqt_str, seq($._br_prp, choice($._r_dqt_str, $._br_dqt_str))),

		_r_sgl_dqt_str_val: $ => choice($._r_sgl_dqt_str, seq($._r_sgl_prp, $._r_sgl_dqt_str)),
		_br_sgl_dqt_str_val: $ => choice($._br_sgl_dqt_str, seq($._br_sgl_prp, $._r_sgl_dqt_str)),
		_b_sgl_dqt_str_val: $ => choice($._b_sgl_dqt_str, seq($._b_sgl_prp, $._r_sgl_dqt_str)),

		_r_dqt_str: $ =>
			seq(
				$._r_dqt_str_bgn,
				optional($._r_sgl_dqt_ctn),
				optional($._r_dqt_esc_nwl),
				repeat($._br_mtl_dqt_ctn),
				choice($._r_dqt_str_end, $._br_dqt_str_end),
			),
		_br_dqt_str: $ =>
			seq(
				$._br_dqt_str_bgn,
				optional($._r_sgl_dqt_ctn),
				optional($._r_dqt_esc_nwl),
				repeat($._br_mtl_dqt_ctn),
				choice($._r_dqt_str_end, $._br_dqt_str_end),
			),

		_r_sgl_dqt_str: $ => seq($._r_dqt_str_bgn, optional($._r_sgl_dqt_ctn), $._r_dqt_str_end),
		_br_sgl_dqt_str: $ => seq($._br_dqt_str_bgn, optional($._r_sgl_dqt_ctn), $._r_dqt_str_end),
		_b_sgl_dqt_str: $ => seq($._b_dqt_str_bgn, optional($._r_sgl_dqt_ctn), $._r_dqt_str_end),

		_r_sgl_dqt_ctn: $ => repeat1(choice($._r_dqt_str_ctn, $._r_dqt_esc_seq, $.delimited_expression)),
		_br_mtl_dqt_ctn: $ =>
			choice(
				$._br_dqt_esc_nwl,
				seq(
					choice($._br_dqt_str_ctn, $._br_dqt_esc_seq, $.delimited_expression),
					repeat(choice($._r_dqt_str_ctn, $._r_dqt_esc_seq, $.delimited_expression)),
					optional($._r_dqt_esc_nwl),
				),
			),

		// single quote scalar

		_r_sqt_str_val: $ => choice($._r_sqt_str, seq($._r_prp, choice($._r_sqt_str, $._br_sqt_str))),
		_br_sqt_str_val: $ => choice($._br_sqt_str, seq($._br_prp, choice($._r_sqt_str, $._br_sqt_str))),

		_r_sgl_sqt_str_val: $ => choice($._r_sgl_sqt_str, seq($._r_sgl_prp, $._r_sgl_sqt_str)),
		_br_sgl_sqt_str_val: $ => choice($._br_sgl_sqt_str, seq($._br_sgl_prp, $._r_sgl_sqt_str)),
		_b_sgl_sqt_str_val: $ => choice($._b_sgl_sqt_str, seq($._b_sgl_prp, $._r_sgl_sqt_str)),

		_r_sqt_str: $ =>
			seq($._r_sqt_str_bgn, optional($._r_sgl_sqt_ctn), repeat($._br_mtl_sqt_ctn), choice($._r_sqt_str_end, $._br_sqt_str_end)),
		_br_sqt_str: $ =>
			seq($._br_sqt_str_bgn, optional($._r_sgl_sqt_ctn), repeat($._br_mtl_sqt_ctn), choice($._r_sqt_str_end, $._br_sqt_str_end)),

		_r_sgl_sqt_str: $ => seq($._r_sqt_str_bgn, optional($._r_sgl_sqt_ctn), $._r_sqt_str_end),
		_br_sgl_sqt_str: $ => seq($._br_sqt_str_bgn, optional($._r_sgl_sqt_ctn), $._r_sqt_str_end),
		_b_sgl_sqt_str: $ => seq($._b_sqt_str_bgn, optional($._r_sgl_sqt_ctn), $._r_sqt_str_end),

		_r_sgl_sqt_ctn: $ => repeat1(choice($._r_sqt_str_ctn, $._r_sqt_esc_sqt, $.delimited_expression)),
		_br_mtl_sqt_ctn: $ =>
			seq(
				choice($._br_sqt_str_ctn, $._br_sqt_esc_sqt, $.delimited_expression),
				repeat(choice($._r_sqt_str_ctn, $._r_sqt_esc_sqt, $.delimited_expression)),
			),

		// plain scalar in block

		_r_pln_blk_val: $ => choice($._r_pln_blk, seq($._r_prp, choice($._r_pln_blk, $._br_pln_blk))),
		_br_pln_blk_val: $ => choice($._br_pln_blk, seq($._br_prp, choice($._r_pln_blk, $._br_pln_blk))),

		_r_sgl_pln_blk_val: $ => choice($._r_sgl_pln_blk, seq($._r_sgl_prp, $._r_sgl_pln_blk)),
		_br_sgl_pln_blk_val: $ => choice($._br_sgl_pln_blk, seq($._br_sgl_prp, $._r_sgl_pln_blk)),
		_b_sgl_pln_blk_val: $ => choice($._b_sgl_pln_blk, seq($._b_sgl_prp, $._r_sgl_pln_blk)),

		_r_pln_blk: $ => choice($._r_sgl_pln_blk, $._r_mtl_pln_blk),
		_br_pln_blk: $ => choice($._br_sgl_pln_blk, $._br_mtl_pln_blk),

		// plain scalar in flow

		_r_pln_flw_val: $ => choice($._r_pln_flw, seq($._r_prp, choice($._r_pln_flw, $._br_pln_flw))),
		_br_pln_flw_val: $ => choice($._br_pln_flw, seq($._br_prp, choice($._r_pln_flw, $._br_pln_flw))),

		_r_sgl_pln_flw_val: $ => choice($._r_sgl_pln_flw, seq($._r_sgl_prp, $._r_sgl_pln_flw)),

		_r_pln_flw: $ => choice($._r_sgl_pln_flw, $._r_mtl_pln_flw),
		_br_pln_flw: $ => choice($._br_sgl_pln_flw, $._br_mtl_pln_flw),

		// plain scalar schema. The string variant in each context allows interleaved
		// `delimited_expression` so that `name: ${{ github.event.title }}` and
		// `prefix-${{ x }}-suffix` parse with native expression nodes. The non-string
		// variants (null/bool/int/float) cannot host expressions — the schema
		// classifier rejects them as soon as `${{` appears in the content.

		_r_sgl_pln_blk: $ =>
			choice(
				$._r_sgl_pln_nul_blk,
				$._r_sgl_pln_bol_blk,
				$._r_sgl_pln_int_blk,
				$._r_sgl_pln_flt_blk,
				prec.right(repeat1(choice($._r_sgl_pln_str_blk, $.delimited_expression))),
			),
		_br_sgl_pln_blk: $ =>
			choice(
				$._br_sgl_pln_nul_blk,
				$._br_sgl_pln_bol_blk,
				$._br_sgl_pln_int_blk,
				$._br_sgl_pln_flt_blk,
				prec.right(repeat1(choice($._br_sgl_pln_str_blk, $.delimited_expression))),
			),
		_b_sgl_pln_blk: $ =>
			choice(
				$._b_sgl_pln_nul_blk,
				$._b_sgl_pln_bol_blk,
				$._b_sgl_pln_int_blk,
				$._b_sgl_pln_flt_blk,
				prec.right(repeat1(choice($._b_sgl_pln_str_blk, $.delimited_expression))),
			),
		_r_sgl_pln_flw: $ =>
			choice(
				$._r_sgl_pln_nul_flw,
				$._r_sgl_pln_bol_flw,
				$._r_sgl_pln_int_flw,
				$._r_sgl_pln_flt_flw,
				prec.right(repeat1(choice($._r_sgl_pln_str_flw, $.delimited_expression))),
			),
		_br_sgl_pln_flw: $ =>
			choice(
				$._br_sgl_pln_nul_flw,
				$._br_sgl_pln_bol_flw,
				$._br_sgl_pln_int_flw,
				$._br_sgl_pln_flt_flw,
				prec.right(repeat1(choice($._br_sgl_pln_str_flw, $.delimited_expression))),
			),

		_r_mtl_pln_blk: $ => prec.right(repeat1(choice($._r_mtl_pln_str_blk, $.delimited_expression))),
		_br_mtl_pln_blk: $ => prec.right(repeat1(choice($._br_mtl_pln_str_blk, $.delimited_expression))),
		_r_mtl_pln_flw: $ => prec.right(repeat1(choice($._r_mtl_pln_str_flw, $.delimited_expression))),
		_br_mtl_pln_flw: $ => prec.right(repeat1(choice($._br_mtl_pln_str_flw, $.delimited_expression))),

		// alias

		_r_als_val: $ => $._r_als,
		_br_als_val: $ => $._br_als,
		_b_als_val: $ => $._b_als,

		_r_als: $ => seq($._r_als_bgn, $._r_als_ctn),
		_br_als: $ => seq($._br_als_bgn, $._r_als_ctn),
		_b_als: $ => seq($._b_als_bgn, $._r_als_ctn),

		// anchor

		_r_acr: $ => seq($._r_acr_bgn, $._r_acr_ctn),
		_br_acr: $ => seq($._br_acr_bgn, $._r_acr_ctn),
		_b_acr: $ => seq($._b_acr_bgn, $._r_acr_ctn),

		// GitHub Actions expression — vendored from hdoc1509/tree-sitter-gh-actions-expressions v0.5.1 (MIT, Héctor Ochoa).
		// Wrapper rules (source/_pair/_if_pair/_run_pair/_ignored_bash) are intentionally omitted; the YAML grammar
		// hosts the entry points by splicing `delimited_expression` (and bare `expression` for `if:` values) into
		// scalar content. Rule bodies are otherwise preserved verbatim so upstream syntax updates can be cherry-picked.

		// Optional horizontal whitespace inside expression bodies. Defined as a private
		// rule (not via `extras`) so it's only valid where expression rules expect it,
		// keeping YAML's structural whitespace handling untouched elsewhere.
		_expr_ws: _ => token(/[ \t\r\n]+/),

		_evaluation: $ => choice($._literal, $.context, $._call, $.logical_group),
		_call: $ => seq($.function_call, repeat($.property)),

		expression: $ =>
			seq(
				$._evaluation,
				repeat(
					seq(
						optional($._expr_ws),
						$.operator,
						optional($._expr_ws),
						optional($.not),
						optional($._expr_ws),
						$._evaluation,
					),
				),
			),
		delimited_expression: $ =>
			seq(
				alias($._expr_bgn, $.expression_marker),
				optional($._expr_ws),
				optional($.not),
				optional($._expr_ws),
				$._evaluation,
				repeat(
					seq(
						optional($._expr_ws),
						$.operator,
						optional($._expr_ws),
						optional($.not),
						optional($._expr_ws),
						$._evaluation,
					),
				),
				optional($._expr_ws),
				"}}",
			),

		_literal: $ => choice($.boolean, $.null, $.number, $.string),

		boolean: () => choice("true", "false"),

		null: () => "null",

		_dot: () => ".",

		number: $ => choice(seq(optional("-"), $._int), $._float, $._hex, $._exp),
		_int: () => /\d+/,
		_float: $ => seq(optional("-"), $._int, $._dot, $._int),
		_hex: () => seq("0x", /[0-9a-fA-F]+/),
		_exp: $ => seq($._float, "e", optional("-"), $._int),

		string: $ => seq("'", repeat(choice($.string_content, $.scape_sequence)), "'"),
		string_content: _ => token(prec(-1, /([^'\\\r\n]|\\(.|\r?\n))+/)),
		scape_sequence: () => token.immediate("''"),
		format_string: $ =>
			seq(
				"'",
				repeat(
					choice(
						$.format_variable,
						alias($._format_scape, $.scape_sequence),
						alias($._format_content, $.string_content),
					),
				),
				"'",
			),
		_format_scape: () => token.immediate(choice("''", "{{", "}}")),
		_format_content: () => token(prec(-1, /([^'{}\\\r\n]|\\(.|\r?\n))+/)),
		format_variable: $ => seq("{", $.format_index, "}"),
		format_index: $ => $._int,

		asterisk: () => "*",
		identifier: () => /[_a-zA-Z][-_a-zA-Z0-9]*/,
		property_deref: $ => $._dot,

		context: $ => seq($.identifier, repeat($.property)),
		property: $ => choice(seq($.property_deref, choice($.identifier, $.asterisk)), $.index),
		index: $ => seq("[", optional($._expr_ws), choice($.number, $.string), optional($._expr_ws), "]"),

		function_call: $ => choice($._format_function, $._function),
		_function: $ =>
			seq(
				field("function", $.identifier),
				"(",
				optional($._expr_ws),
				field("arguments", optional($.arguments)),
				optional($._expr_ws),
				")",
			),
		arguments: $ =>
			seq(
				$._evaluation,
				repeat(seq(optional($._expr_ws), ",", optional($._expr_ws), $._evaluation)),
			),
		_format_function: $ =>
			seq(
				field("function", alias("format", $.identifier)),
				"(",
				optional($._expr_ws),
				field("arguments", optional(alias($._format_arguments, $.arguments))),
				optional($._expr_ws),
				")",
			),
		_format_arguments: $ =>
			seq(
				$.format_string,
				repeat(seq(optional($._expr_ws), ",", optional($._expr_ws), $._evaluation)),
			),

		not: () => "!",
		operator: $ => choice($.lt, $.le, $.gt, $.ge, $.eq, $.ne, $.and, $.or),
		lt: () => "<",
		le: () => "<=",
		gt: () => ">",
		ge: () => ">=",
		eq: () => "==",
		ne: () => "!=",
		and: () => "&&",
		or: () => "||",

		logical_group: $ =>
			seq(
				"(",
				optional($._expr_ws),
				optional($.not),
				optional($._expr_ws),
				$._evaluation,
				repeat(
					seq(
						optional($._expr_ws),
						$.operator,
						optional($._expr_ws),
						optional($.not),
						optional($._expr_ws),
						$._evaluation,
					),
				),
				optional($._expr_ws),
				")",
			),
	},
});
const aliasedSchema = global_alias(
	global_alias(base.grammar, {
		..._("yaml_directive", "_s_dir_yml"),
		..._("yaml_version", "_r_dir_yml_ver"),
		..._("tag_directive", "_s_dir_tag"),
		..._("tag_handle", "_r_dir_tag_hdl"),
		..._("tag_prefix", "_r_dir_tag_pfx"),
		..._("reserved_directive", "_s_dir_rsv"),
		..._("directive_name", "_s_dir_rsv_bgn"),
		..._("directive_parameter", "_r_dir_rsv_prm"),
		..._("flow_node", "_r_prp_val", "_br_prp_val", "_r_sgl_prp_val", "_br_sgl_prp_val", "_b_sgl_prp_val"),
		..._("tag", "_r_tag", "_br_tag", "_b_tag"),
		..._("anchor", "_r_acr", "_br_acr", "_b_acr"),
		..._("anchor_name", "_r_acr_ctn"),
		..._("flow_node", "_r_als_val", "_br_als_val", "_b_als_val"),
		..._("alias", "_r_als", "_br_als", "_b_als"),
		..._("alias_name", "_r_als_ctn"),
		..._("document", "_bgn_imp_doc", "_imp_doc"),
		..._(["document"], "_drs_doc", "_exp_doc", "_doc_end", "_bgn_imp_doc_end", "_drs_doc_end", "_exp_doc_end", "_imp_doc_end"),
		..._(
			"block_node",
			"_r_blk_seq_r_val",
			"_r_blk_seq_br_val",
			"_br_blk_seq_val",
			"_r_blk_seq_spc_val",
			"_br_blk_seq_spc_val",
			"_b_blk_seq_spc_val",
		),
		..._("block_node", "_r_blk_map_r_val", "_r_blk_map_br_val", "_br_blk_map_val"),
		..._("block_node", "_r_blk_str_val", "_br_blk_str_val"),
		..._("block_sequence", "_r_blk_seq", "_br_blk_seq", "_b_blk_seq_spc"),
		..._("block_mapping", "_r_blk_map", "_br_blk_map"),
		..._("block_scalar", "_r_blk_str", "_br_blk_str"),
		..._("block_sequence_item", "_r_blk_seq_itm", "_br_blk_seq_itm", "_b_blk_seq_itm"),
		..._("block_mapping_pair", "_r_blk_exp_itm", "_br_blk_exp_itm", "_b_blk_exp_itm"),
		..._("block_mapping_pair", "_r_blk_imp_itm", "_br_blk_imp_itm", "_b_blk_imp_itm"),
		..._("flow_node", "_r_flw_seq_val", "_br_flw_seq_val", "_r_sgl_flw_seq_val", "_br_sgl_flw_seq_val", "_b_sgl_flw_seq_val"),
		..._("flow_node", "_r_flw_map_val", "_br_flw_map_val", "_r_sgl_flw_map_val", "_br_sgl_flw_map_val", "_b_sgl_flw_map_val"),
		..._("flow_sequence", "_r_flw_seq", "_br_flw_seq", "_r_sgl_flw_seq", "_br_sgl_flw_seq", "_b_sgl_flw_seq"),
		..._("flow_mapping", "_r_flw_map", "_br_flw_map", "_r_sgl_flw_map", "_br_sgl_flw_map", "_b_sgl_flw_map"),
		..._(
			["flow_pair"],
			"_r_flw_exp_par",
			"_br_flw_exp_par",
			"_r_sgl_flw_exp_par",
			"_r_flw_imp_r_par",
			"_r_flw_imp_br_par",
			"_br_flw_imp_r_par",
			"_br_flw_imp_br_par",
			"_r_sgl_flw_imp_par",
			"_r_flw_njl_ann_par",
			"_br_flw_njl_ann_par",
			"_r_sgl_flw_njl_ann_par",
		),
		..._("flow_node", "_r_dqt_str_val", "_br_dqt_str_val", "_r_sgl_dqt_str_val", "_br_sgl_dqt_str_val", "_b_sgl_dqt_str_val"),
		..._("flow_node", "_r_sqt_str_val", "_br_sqt_str_val", "_r_sgl_sqt_str_val", "_br_sgl_sqt_str_val", "_b_sgl_sqt_str_val"),
		..._(
			"flow_node",
			"_r_pln_blk_val",
			"_br_pln_blk_val",
			"_r_sgl_pln_blk_val",
			"_br_sgl_pln_blk_val",
			"_b_sgl_pln_blk_val",
			"_r_pln_flw_val",
			"_br_pln_flw_val",
			"_r_sgl_pln_flw_val",
			"_r_if_expr_val",
		),
		..._("double_quote_scalar", "_r_dqt_str", "_br_dqt_str", "_r_sgl_dqt_str", "_br_sgl_dqt_str", "_b_sgl_dqt_str"),
		..._("single_quote_scalar", "_r_sqt_str", "_br_sqt_str", "_r_sgl_sqt_str", "_br_sgl_sqt_str", "_b_sgl_sqt_str"),
		..._(
			"plain_scalar",
			"_r_mtl_pln_blk",
			"_br_mtl_pln_blk",
			"_r_sgl_pln_blk",
			"_br_sgl_pln_blk",
			"_b_sgl_pln_blk",
			"_r_mtl_pln_flw",
			"_br_mtl_pln_flw",
			"_r_sgl_pln_flw",
			"_br_sgl_pln_flw",
		),
		..._("escape_sequence", "_r_dqt_esc_nwl", "_br_dqt_esc_nwl", "_r_dqt_esc_seq", "_br_dqt_esc_seq", "_r_sqt_esc_sqt", "_br_sqt_esc_sqt"),
		..._("null_scalar", "_r_sgl_pln_nul_blk", "_br_sgl_pln_nul_blk", "_b_sgl_pln_nul_blk", "_r_sgl_pln_nul_flw", "_br_sgl_pln_nul_flw"),
		..._("boolean_scalar", "_r_sgl_pln_bol_blk", "_br_sgl_pln_bol_blk", "_b_sgl_pln_bol_blk", "_r_sgl_pln_bol_flw", "_br_sgl_pln_bol_flw"),
		..._("integer_scalar", "_r_sgl_pln_int_blk", "_br_sgl_pln_int_blk", "_b_sgl_pln_int_blk", "_r_sgl_pln_int_flw", "_br_sgl_pln_int_flw"),
		..._("float_scalar", "_r_sgl_pln_flt_blk", "_br_sgl_pln_flt_blk", "_b_sgl_pln_flt_blk", "_r_sgl_pln_flt_flw", "_br_sgl_pln_flt_flw"),
		..._(
			"string_scalar",
			"_r_sgl_pln_str_blk",
			"_br_sgl_pln_str_blk",
			"_b_sgl_pln_str_blk",
			"_r_sgl_pln_str_flw",
			"_br_sgl_pln_str_flw",
			"_r_mtl_pln_str_blk",
			"_br_mtl_pln_str_blk",
			"_r_mtl_pln_str_flw",
			"_br_mtl_pln_str_flw",
		),
	}),
	{
		..._("---", "_s_drs_end"),
		..._("...", "_s_doc_end"),
		..._("-", "_r_blk_seq_bgn", "_br_blk_seq_bgn", "_b_blk_seq_bgn"),
		..._("?", "_r_blk_key_bgn", "_br_blk_key_bgn", "_b_blk_key_bgn"),
		..._(":", "_r_blk_val_bgn", "_br_blk_val_bgn", "_b_blk_val_bgn"),
		..._(":", "_r_blk_imp_bgn"),
		..._("|", "_r_blk_lit_bgn", "_br_blk_lit_bgn"),
		..._(">", "_r_blk_fld_bgn", "_br_blk_fld_bgn"),
		..._("[", "_r_flw_seq_bgn", "_br_flw_seq_bgn", "_b_flw_seq_bgn"),
		..._("]", "_r_flw_seq_end", "_br_flw_seq_end", "_b_flw_seq_end"),
		..._("{", "_r_flw_map_bgn", "_br_flw_map_bgn", "_b_flw_map_bgn"),
		..._("}", "_r_flw_map_end", "_br_flw_map_end", "_b_flw_map_end"),
		..._(",", "_r_flw_sep_bgn", "_br_flw_sep_bgn"),
		..._("?", "_r_flw_key_bgn", "_br_flw_key_bgn"),
		..._(":", "_r_flw_jsv_bgn", "_br_flw_jsv_bgn"),
		..._(":", "_r_flw_njv_bgn", "_br_flw_njv_bgn"),
		..._("\"", "_r_dqt_str_bgn", "_br_dqt_str_bgn", "_b_dqt_str_bgn"),
		..._("\"", "_r_dqt_str_end", "_br_dqt_str_end"),
		..._("'", "_r_sqt_str_bgn", "_br_sqt_str_bgn", "_b_sqt_str_bgn"),
		..._("'", "_r_sqt_str_end", "_br_sqt_str_end"),
		..._("*", "_r_als_bgn", "_br_als_bgn", "_b_als_bgn"),
		..._("&", "_r_acr_bgn", "_br_acr_bgn", "_b_acr_bgn"),
	},
);

/**
 * @param {string | [string]} alias_value - alias name (string) or `[name]` to mark the alias as shallow
 * @param {...string} rule_names - rules that should adopt this alias
 * @returns {AliasMap}
 */
function _(alias_value, ...rule_names) {
	/** @type {AliasContent} */
	let alias_content;
	if (typeof alias_value === "string") {
		alias_content = { name: alias_value };
	} else if (Array.isArray(alias_value)) {
		alias_content = { name: alias_value[0], shallow: true };
	} else {
		throw new Error(`Unexpected value ${JSON.stringify(alias_value)}`);
	}
	/** @type {AliasMap} */
	const alias_map = {};
	for (const rule_name of rule_names) {
		alias_map[rule_name] = alias_content;
	}
	return alias_map;
}

/**
 * @param {GrammarJson} grammar_json - resolved grammar JSON to alias
 * @param {AliasMap} alias_map - rules to rewrite as aliases
 * @returns {GrammarJson} new grammar with aliases applied
 */
function global_alias(grammar_json, alias_map) {
	/** @type {Record<string, RuleJson>} */
	const new_rules = {};
	const new_grammar = { ...grammar_json, rules: new_rules };
	/** @type {Record<string, number>} */
	const checklist = Object.fromEntries(Object.keys(alias_map).map(k => [k, 0]));
	for (const [rule_name, rule] of Object.entries(grammar_json.rules)) {
		new_rules[rule_name] = rule_name in alias_map && alias_map[rule_name]?.shallow
			? rule
			: recursive_alias(rule, alias_map, checklist);
	}
	for (const [rule_name, counter] of Object.entries(checklist)) {
		if (counter === 0) {
			console.warn(`warning: global_alias for ${JSON.stringify(rule_name)} is not used.`);
		}
	}
	return new_grammar;
}

/**
 * @param {RuleJson} rule - rule to walk and rewrite
 * @param {AliasMap} alias_map - rules that should be rewritten as aliases
 * @param {Record<string, number>} checklist - usage counter, mutated as aliases are applied
 * @returns {RuleJson}
 */
function recursive_alias(rule, alias_map, checklist) {
	switch (rule.type) {
		case "CHOICE":
		case "SEQ":
			return { ...rule, members: (rule.members ?? []).map(member => recursive_alias(member, alias_map, checklist)) };
		case "REPEAT":
		case "REPEAT1":
		case "FIELD":
		case "PREC":
		case "PREC_LEFT":
		case "PREC_RIGHT":
		case "PREC_DYNAMIC":
			return { ...rule, content: rule.content && recursive_alias(rule.content, alias_map, checklist) };
		case "SYMBOL": {
			const name = rule.name;
			if (name && name in alias_map) {
				const entry = alias_map[name];
				if (entry) {
					checklist[name] = (checklist[name] ?? 0) + 1;
					return { type: "ALIAS", content: rule, named: /[a-z]/i.test(entry.name), value: entry.name };
				}
			}
			return rule;
		}
		case "BLANK":
		case "ALIAS":
		case "STRING":
		case "PATTERN":
		case "TOKEN":
		case "IMMEDIATE_TOKEN":
			return rule;
		default:
			throw new Error(`Unexpected rule type ${JSON.stringify(rule.type)}`);
	}
}

export default { grammar: aliasedSchema };

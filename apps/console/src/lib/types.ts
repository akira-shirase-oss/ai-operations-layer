// The collector's JSON, mirrored from crates/aiops-domain/src/timeline.rs and
// crates/aiops-server/src/query.rs. Every field the console renders is data from an
// attacker-influenced source: it is shown as text, never as HTML.

export type State = 'succeeded' | 'failed' | 'uncertain' | 'conflicting' | 'no_finish_observed';

export type Status = {
	state: State;
	class?: string;
	reason?: string;
	declared: boolean;
};

export type Flags = {
	failed_attempts: number;
	uncertain_attempts: number;
	retried_steps: number;
	external_effect_unknown: boolean;
	clock_skew_suspected: boolean;
	late_events: number;
	conflicting_finish: boolean;
	start_not_observed: boolean;
};

export type InstanceCoverage = {
	instance_id: string;
	emitted: number | null;
	received: number;
	missing_op_seq: number[];
	missing_known: number;
	tail_unknown: boolean;
};

export type Coverage = {
	events_received: number;
	sdk_gap_reports: number;
	reported_dropped: number;
	missing_known: number;
	missing_unknown: boolean;
	expired: number;
	per_instance: InstanceCoverage[];
};

export type CostView = {
	label: 'estimate';
	estimate_micros: number;
	currency: string | null;
	mixed_currencies: boolean;
	attempts_with_estimate: number;
	input_tokens: number;
	output_tokens: number;
};

export type AttrView = { key: string; value: unknown; opt_in: boolean };

export type EntryView = {
	event_id: string;
	type: string;
	instance_id: string;
	ingest_key: string;
	seq: number;
	op_seq: number;
	event_time: string;
	received_at: string;
	receive_delay_us: number;
	flags: string[];
	step_id?: string;
	attempt?: number;
	kind?: string;
	name?: string;
	outcome?: Status;
	dropped?: number;
	reason?: string;
	attributes: AttrView[];
	withheld_attributes: number;
	redactions: number;
	overflowed_names: number;
	truncated: number;
};

export type AttemptView = {
	attempt: number;
	state: State;
	class?: string;
	reason?: string;
	start_observed: boolean;
	finish_observed: boolean;
	started_at?: string;
	finished_at?: string;
	duration_us?: number;
	model?: string;
	prompt_version?: string;
	config_version?: string;
	error_code?: string;
	error_message?: string;
	input_tokens?: number;
	output_tokens?: number;
	cost_estimate_micros?: number;
	cost_currency?: string;
};

export type StepView = {
	step_id: string;
	parent_step_id?: string;
	kind: string | null;
	name: string | null;
	external_effect: boolean;
	first_event_time: string;
	attempts: AttemptView[];
};

export type GapView = {
	kind: 'sdk_reported' | 'op_seq_hole' | 'unknown_tail' | 'expired';
	instance_id: string;
	count: number | null;
	reason?: string;
	from?: string;
	to?: string;
	op_seq_from?: number;
	op_seq_to?: number;
};

export type SourceView = {
	instance_id: string;
	service: string;
	sdk: string;
	ingest_key: string;
	first_seen: string;
	last_seen: string;
	missing_by_seq: number;
	events_refused: number;
	reported_dropped_queue_full: number;
	reported_dropped_send_failed: number;
	reported_dropped_rejected: number;
	reported_gap_overflow: number;
};

export type EffectCause = {
	step_id: string;
	name: string | null;
	attempt: number;
	cause: 'uncertain' | 'no_finish_observed';
	declared: 'yes' | 'unknown';
};

export type Timeline = {
	operation_id: string;
	name: string | null;
	request_id: string | null;
	trace_id: string | null;
	release: string | null;
	deployment: string | null;
	status: Status;
	flags: Flags;
	coverage: Coverage;
	cost: CostView;
	steps: StepView[];
	entries: EntryView[];
	gaps: GapView[];
	sources: SourceView[];
	effect_unknown_steps: EffectCause[];
	expired_by_retention: boolean;
	retention_cutoff: string | null;
	events_truncated: boolean;
};

export type OperationSummary = {
	operation_id: string;
	name: string | null;
	request_id: string | null;
	release: string | null;
	status: Status;
	first_event_time: string;
	last_event_time: string;
	last_received_at: string;
	events_received: number;
	events_emitted: number | null;
	events_missing: number | null;
	reported_dropped: number;
	failed_attempts: number;
	uncertain_attempts: number;
	cost_estimate_micros: number;
	cost_currency: string | null;
	detail_expired: boolean;
	multi_source: boolean;
};

export type OperationList = {
	operations: OperationSummary[];
	next: [string, string] | null;
};

export type ProjectsView = {
	actor: string;
	scopes: string[];
	projects: { slug: string }[];
};

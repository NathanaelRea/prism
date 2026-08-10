import { readFile } from "node:fs/promises";

export const TRIGGER_PROTOCOL_VERSION = 1 as const;

export interface TriggerSubject {
  repository: string;
  worktree: string;
  change_request?: string | null;
  /** Initial launch-head hint. Trigger observations must use the current provider head. */
  change_request_head?: string | null;
}

export interface TriggerContext {
  run_id: string;
  step_key: string;
  attempt_id: string;
  cycle: number;
  cycle_started_unix_ms: number;
  subject: TriggerSubject;
  cancellation_requested: boolean;
}

export type TriggerDecision =
  | { decision: "run"; summary: string }
  | { decision: "satisfied"; summary: string }
  | { decision: "wait"; summary: string; wake_at_unix_ms: number }
  | { decision: "fail"; reason: string };

export type PreparedState = unknown;

export interface AgentOutcome {
  status: "succeeded" | "failed" | "cancelled";
  process_id?: number | null;
  session_id: string;
  final_text: string;
}

export type PostStepResult =
  | { result: "success"; summary: string }
  | { result: "wait"; summary: string; wake_at_unix_ms: number }
  | { result: "fail"; reason: string };

export type TriggerRequest =
  | {
      protocol_version: 1;
      phase: "should_run_step";
      context: TriggerContext;
    }
  | {
      protocol_version: 1;
      phase: "pre_step_run";
      context: TriggerContext;
    }
  | {
      protocol_version: 1;
      phase: "post_step_run";
      context: TriggerContext;
      prepared_state: PreparedState;
      agent_outcome: AgentOutcome;
    };

export interface Trigger {
  shouldRunStep(context: TriggerContext): TriggerDecision | Promise<TriggerDecision>;
  preStepRun?(context: TriggerContext): PreparedState | Promise<PreparedState>;
  postStepRun?(
    context: TriggerContext,
    preparedState: PreparedState,
    agentOutcome: AgentOutcome,
  ): PostStepResult | Promise<PostStepResult>;
}

/**
 * Read one bounded Prism request and write exactly one protocol response.
 * Diagnostics belong on stderr; stdout is reserved for this response.
 */
export async function runTrigger(trigger: Trigger, maximumInputBytes = 64 * 1024): Promise<void> {
  const input = await readFile(0);
  if (input.byteLength > maximumInputBytes) {
    throw new Error(`Trigger request exceeded ${maximumInputBytes} bytes`);
  }
  const request = JSON.parse(input.toString("utf8")) as TriggerRequest;
  if (request.protocol_version !== TRIGGER_PROTOCOL_VERSION) {
    throw new Error(`Unsupported Trigger protocol version ${String(request.protocol_version)}`);
  }

  let response: object;
  switch (request.phase) {
    case "should_run_step":
      response = {
        response: "decision",
        protocol_version: TRIGGER_PROTOCOL_VERSION,
        decision: await trigger.shouldRunStep(request.context),
      };
      break;
    case "pre_step_run":
      response = {
        response: "prepared",
        protocol_version: TRIGGER_PROTOCOL_VERSION,
        prepared_state: trigger.preStepRun
          ? await trigger.preStepRun(request.context)
          : null,
      };
      break;
    case "post_step_run":
      response = {
        response: "completed",
        protocol_version: TRIGGER_PROTOCOL_VERSION,
        completion: trigger.postStepRun
          ? await trigger.postStepRun(
              request.context,
              request.prepared_state,
              request.agent_outcome,
            )
          : { result: "success", summary: "finalized" },
      };
      break;
  }
  process.stdout.write(`${JSON.stringify(response)}\n`);
}

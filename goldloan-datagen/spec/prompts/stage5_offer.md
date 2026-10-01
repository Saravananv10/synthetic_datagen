# Stage 5 — Offer Selection (scheme, quote, income, bureau, eligibility, confirmation)
Goal: let the customer choose a scheme, quote it, capture income when required, check the bureau score and eligibility, and confirm the offer.

FIRST REPLY ONLY: your first input is the user's `PROCEED`. Call `get_offers(trigger="trigger")` and reply with one line `Available schemes:` then a table with columns `Offer ID | Scheme Type | ROI | Tenure | Repayment` (ROI as `12.0%`, tenure as `12m`) built only from the tool result, then `Which offer did the customer select?` — no buttons on this reply; the user types the offer id as free text.

## Flow (this stage deliberately advances one step per user "Proceed")
1. Selection. When the user names an offer id: call `select_scheme(offer_id=<id>)` and then `calculate_offer(action="quote", tenure_months="<tenure>")`. Present `The customer's plan is as follows:` then a table `Plan Details | Value` with `Loan Amount`, `ROI`, `Tenure`, `Monthly EMI`, `APR`, `LTV` from the tool, then buttons `CONFIRM/CHANGE/EXIT`. (For a Bullet scheme show `Monthly Interest` instead of `Monthly EMI` if the tool returns that.)
   - `INVALID_OFFER`: say that offer id is not available and re-ask.
2. CHANGE. Ask whether the customer wants a different scheme or a lower ROI. For a lower ROI call `calculate_offer(action="negotiate", tenure_months="<tenure>", requested_roi=<roi>)` and present the tool's result the same way as a quote (or explain the counter-offer / refusal in one sentence), again with `CONFIRM/CHANGE/EXIT`. For a different scheme go back to step 1.
3. CONFIRM. Call `calculate_offer(action="confirm", tenure_months="<tenure>")`.
   - `INCOME_REQUIRED`: reply `The loan amount exceeds Rs 250,000, so income capture is required. Please provide the customer's employment type.` with buttons `SALARIED/SELF-EMPLOYED/EXIT`.
   - `BUREAU_REQUIRED`: reply `The system requires a bureau check before the final confirmation. I will fetch the customer's bureau score.` (no buttons, no tool call this turn — the user will type Proceed).
   - success: go to step 8.
4. Employment type. Call `capture_income(employment_type="salaried")` for SALARIED. If the tool succeeds reply `Employment type salaried saved. Please upload the customer's last 3 consecutive monthly payslips.` For SELF-EMPLOYED call `capture_income(employment_type="self_employed")`; if it returns `UNSUPPORTED_EMPLOYMENT_TYPE` tell the user this application must be handled at the branch (manual underwriting) and offer `EXIT` only.
5. Payslips. You receive a text `[PAYSLIP SUMMARY] Payslips: n/3 Months: ... Consecutive: Yes|No Date range: Valid|Expired Name match: Yes|No Status: PASS|FAIL Action: ...`.
   - `Status: PASS`: call `collect_payslips(trigger="trigger")`, then `calculate_offer(action="confirm", tenure_months="<tenure>")` immediately and handle its result as in step 3.
   - `Status: FAIL`: call no tool; say what is wrong (missing month, not consecutive, too old, name mismatch) and ask for corrected payslips.
6. When the user types Proceed after the bureau notice: call `get_bureau_score(trigger="trigger")` and reply `The customer's CIBIL score is <score> (<tier>). I am now checking the final loan eligibility.` (no buttons). If `eligible_for_loan` is false reply that the customer is not eligible because of the bureau score and offer `EXIT` only.
7. When the user types Proceed: call `check_loan_eligibility(trigger="trigger")`. On success reply `Eligibility confirmed for Rs <amount>. I am now finalizing the offer.` (no buttons). On failure (`NOT_ELIGIBLE`, `FOIR_EXCEEDED`) explain in one sentence and offer `EXIT` only. When the user types Proceed again call `calculate_offer(action="confirm", tenure_months="<tenure>")`.
8. On confirm success reply `The offer has been confirmed at the full eligible amount of Rs <loan_amount>.` with buttons `PROCEED/EXIT`.

//! Human-readable capability summaries for Composio toolkit slugs, plus what
//! the toolkit's actions hand back.

/// Human-readable capability summary for a Composio toolkit slug.
///
/// Used by the prompt renderer to tell the orchestrator what each connected
/// integration can do. Covers the most common toolkits; unknown slugs get
/// a generic fallback so newly connected services still appear.
pub fn toolkit_description(slug: &str) -> &'static str {
    match slug {
        "gmail" => {
            "Send, read, draft, reply, forward, and search emails; manage labels and threads"
        }
        "notion" => "Create, read, update, and search notion pages and notion databases",
        "github" => {
            "Manage repositories, issues, and pull requests on GitHub; sync \
             assigned issues into Memory Tree"
        }
        "slack" => "Send messages, read channels, manage threads, and post updates in Slack",
        "discord" => "Send messages, manage channels, and interact with Discord servers",
        "google_calendar" | "googlecalendar" => {
            "Create, update, and query calendar events; check availability"
        }
        "google_drive" | "googledrive" => {
            "Upload, download, search, and share files in Google Drive"
        }
        "google_docs" | "googledocs" => "Create, read, and edit Google Docs documents",
        "google_sheets" | "googlesheets" => "Read, write, and manage Google Sheets spreadsheets",
        "outlook" => "Send, read, and manage emails in Microsoft Outlook",
        "microsoft" | "microsoft_teams" => "Send messages and manage channels in Microsoft Teams",
        "larksuite" => {
            "Connect Lark / Feishu workspace chat, docs, wiki, and meetings via Composio"
        }
        "linear" => {
            "Create, read, and manage issues, projects, and cycles in Linear; sync \
             assigned issues into Memory Tree"
        }
        "jira" => "Create and manage issues, projects, and sprints in Jira",
        "trello" => "Create and manage cards, lists, and boards in Trello",
        "asana" => "Create and manage tasks, projects, and sections in Asana",
        "clickup" => {
            "Create, read, and manage tasks, lists, and docs in ClickUp; sync \
             assigned tasks into Memory Tree"
        }
        "dropbox" => "Upload, download, and share files in Dropbox",
        "twitter" => "Post tweets, read timelines, and manage Twitter interactions",
        "spotify" => "Control playback, search music, and manage playlists on Spotify",
        "telegram" => "Send and receive messages via Telegram",
        "whatsapp" => "Send and receive messages via WhatsApp",
        "twilio" => "Send SMS, make calls, and manage communications via Twilio",
        "shopify" => "Manage products, orders, and customers in Shopify",
        "stripe" => "Manage payments, subscriptions, and customers in Stripe",
        "hubspot" => "Manage contacts, deals, and marketing in HubSpot",
        "salesforce" => "Manage contacts, leads, and opportunities in Salesforce",
        "airtable" => "Read and write records in Airtable bases",
        "figma" => "Access and manage Figma design files and components",
        "youtube" => "Search videos, manage playlists, and interact with YouTube",
        "calendar" => "Create, update, and query calendar events",
        "one_drive" | "onedrive" | "one" => {
            "Upload, download, search, and share files in Microsoft OneDrive"
        }
        "excel" => "Read, write, and manage workbooks, worksheets, and tables in Microsoft Excel",
        "todoist" => "Create and manage tasks, projects, sections, and labels in Todoist",
        _ => "Interact with this connected service via its available actions",
    }
}

/// What a toolkit's actions hand back, and which field feeds which follow-up
/// action. `None` for a toolkit we have not established this for.
///
/// [`toolkit_description`] answers "what can this service do", which is an
/// **input**-side question — and so is everything else the model reads before
/// calling: the tool catalogue, the parameter schema. Nothing tells it what
/// comes back. So a list action returns records keyed by id, the model has no
/// statement that the id is the handle for the detail it actually wanted, and it
/// re-issues the same list call. That was observed live against Gmail.
///
/// The rule for adding an entry: name only action slugs this crate's curated
/// catalogues carry, and say only what a caller has established by observing
/// those actions. A toolkit nobody has checked gets no entry — a guess about a
/// response is worse here than silence, because the model will act on it.
///
/// **Do not describe field-by-field record shapes here.** A note may say what a
/// result *contains* and what to do with it, not how it is serialized. Composio
/// dispatch prefers the backend's rendered `markdownFormatted` body and falls
/// back to the JSON envelope only when that is absent, so a note reciting JSON
/// keys is true on one of two renderings. An earlier revision of this text made
/// exactly that mistake and told the model every Gmail read action answers with
/// a markdown body, when only `GMAIL_FETCH_EMAILS` carries one.
pub fn toolkit_result_notes(slug: &str) -> Option<&'static str> {
    match slug {
        // Slugs: `gmail::GMAIL_CURATED`.
        //
        // The thread/message distinction is the whole point of this entry. Live,
        // a sub-agent searched with GMAIL_LIST_THREADS, got no message body back,
        // and reported that mail which does exist could not be found.
        "gmail" => Some(
            "GMAIL_LIST_THREADS answers with thread ids, a one-line snippet, and a message \
             count — never a message body, so a thread whose snippet looks right still has \
             to be read. Pass a thread id to GMAIL_FETCH_MESSAGE_BY_THREAD_ID, or a message \
             id to GMAIL_FETCH_MESSAGE_BY_MESSAGE_ID, to get the body; GMAIL_FETCH_EMAILS \
             carries one already. Bodies are the backend's rendered text, not the raw \
             message, and attachments arrive as a filename and type that GMAIL_GET_ATTACHMENT \
             fetches. Repeating a search returns the same snippets, so read the thread \
             instead of searching again.",
        ),
        // Slugs: `messaging::SLACK_CURATED`.
        "slack" => Some(
            "SLACK_LIST_CONVERSATIONS answers with a channel id per channel, and that id is \
             the channel argument SLACK_FETCH_CONVERSATION_HISTORY and the post actions take. \
             History entries identify their author by Slack user id, not display name, so \
             resolve it with SLACK_FIND_USERS before quoting a name, and identify themselves \
             by a ts timestamp, which is what threads and reactions key on.",
        ),
        _ => None,
    }
}

#[cfg(test)]
#[path = "descriptions_tests.rs"]
mod tests;

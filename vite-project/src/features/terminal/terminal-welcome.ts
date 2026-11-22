const WIDE_BANNER_MIN_COLUMNS = 76;

const WIDE_BANNER = [
    " _       ____ __  __ ____    ___  __  __",
    "| |     / ___|\\ \\/ /| __ )  / _ \\ \\ \\/ /",
    "| |    | |     \\  / |  _ \\ | | | | \\  /",
    "| |___ | |___  /  \\ | |_) || |_| | /  \\",
    "|_____| \\____|/_/\\_\\|____/  \\___/ /_/\\_\\",
    "",
    "    _                         _      ____              _",
    "   / \\     __ _   ___  _ __  | |_   |  _ \\   ___  ___ | | __",
    "  / _ \\   / _` | / _ \\| '_ \\ | __|  | | | | / _ \\/ __|| |/ /",
    " / ___ \\ | (_| ||  __/| | | || |_   | |_| ||  __/\\__ \\|   <",
    "/_/   \\_\\ \\__, | \\___||_| |_| \\__|  |____/  \\___||___/|_|\\_\\",
    "          |___/",
];

const COMPACT_BANNER = [
    "+-------------------+",
    "| LCXBOX Agent Desk |",
    "+-------------------+",
];

export function terminalWelcomeBanner(columns: number): string {
    const lines = columns >= WIDE_BANNER_MIN_COLUMNS ? WIDE_BANNER : COMPACT_BANNER;
    return `\x1b[36m${lines.join("\r\n")}\x1b[0m\r\n\r\n`;
}

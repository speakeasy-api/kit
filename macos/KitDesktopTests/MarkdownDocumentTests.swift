import XCTest
@testable import Kit

final class MarkdownDocumentTests: XCTestCase {
    func testParsesBlockMarkdownWithoutFlatteningStructure() {
        let source = """
        # Markdown

        - Item 1
        - Item 2

        **Bold** and *italic*.

        ```javascript
        console.log(\"hello\");
        ```
        """

        XCTAssertEqual(MarkdownDocument(source).blocks, [
            .heading(level: 1, text: "Markdown"),
            .unordered(["Item 1", "Item 2"]),
            .paragraph("**Bold** and *italic*."),
            .code(language: "javascript", text: "console.log(\"hello\");"),
        ])
    }

    func testPreservesParagraphAndQuoteLineBreaks() {
        XCTAssertEqual(MarkdownDocument("first\nsecond\n\n> quote\n> next").blocks, [
            .paragraph("first\nsecond"),
            .quote("quote\nnext"),
        ])
    }

    func testParsesMarkdownTablesAndEscapedPipes() {
        let source = """
        | Name | Result |
        | :--- | ---: |
        | compose | **ok** |
        | a\\|b | `value` |
        | C:\\tmp | path |
        """

        XCTAssertEqual(MarkdownDocument(source).blocks, [
            .table(
                headers: ["Name", "Result"],
                alignments: [.leading, .trailing],
                rows: [
                    ["compose", "**ok**"], ["a|b", "`value`"], ["C:\\tmp", "path"],
                ]
            ),
        ])
    }
}

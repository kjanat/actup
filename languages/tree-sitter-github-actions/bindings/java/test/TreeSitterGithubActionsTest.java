import io.github.treesitter.jtreesitter.Language;
import io.github.treesitter.jtreesitter.githubactions.TreeSitterGithubActions;
import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;

public class TreeSitterGithubActionsTest {
    @Test
    public void testCanLoadLanguage() {
        assertDoesNotThrow(() -> new Language(TreeSitterGithubActions.language()));
    }
}
